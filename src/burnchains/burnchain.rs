/*
 copyright: (c) 2013-2018 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::path::PathBuf;
use std::fs;
use std::thread;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use std::collections::HashMap;
use std::collections::HashSet;

use address::AddressHashMode;
use address::public_keys_to_address_hash;

use burnchains::Address;
use burnchains::PublicKey;
use burnchains::BurnchainHeaderHash;
use burnchains::Burnchain;
use burnchains::Txid;
use burnchains::{
    BurnchainSigner,
    BurnchainRecipient,
    BurnchainTransaction,
    BurnchainBlock,
    BurnchainBlockHeader,
    BurnchainParameters,
    Error as burnchain_error,
    BurnchainStateTransition,
    BurnchainStateTransitionOps,
    PoxConstants
};

use burnchains::db::{
    BurnchainDB
};

use burnchains::indexer::{BurnchainIndexer, BurnchainBlockParser, BurnchainBlockDownloader, BurnBlockIPC};

use burnchains::bitcoin::{BitcoinTxInput, BitcoinTxOutput, BitcoinInputType};
use burnchains::bitcoin::address::to_c32_version_byte;
use burnchains::bitcoin::address::address_type_to_version_byte;
use burnchains::bitcoin::address::BitcoinAddress;
use burnchains::bitcoin::address::BitcoinAddressType;
use burnchains::bitcoin::BitcoinNetworkType;

use chainstate::burn::{ 
    Opcodes, BlockSnapshot
};
use chainstate::burn::distribution::BurnSamplePoint;
use chainstate::burn::operations::{
    LeaderBlockCommitOp,
    LeaderKeyRegisterOp,
    UserBurnSupportOp,
    BlockstackOperation,
    BlockstackOperationType,
};
use chainstate::burn::db::sortdb::{
    SortitionDB, SortitionHandleTx, SortitionHandleConn,
    PoxId
};

use chainstate::coordinator::comm::CoordinatorChannels;

use chainstate::stacks::StacksAddress;
use chainstate::stacks::StacksPublicKey;
use chainstate::stacks::index::TrieHash;

use util::db::Error as db_error;
use util::log;
use util::hash::to_hex;
use util::get_epoch_time_ms;
use util::db::DBConn;
use util::db::DBTx;
use util::vrf::VRFPublicKey;
use util::get_epoch_time_secs;

use core::PEER_VERSION;
use core::NETWORK_ID_MAINNET;
use core::NETWORK_ID_TESTNET;

use burnchains::bitcoin::indexer::FIRST_BLOCK_MAINNET as BITCOIN_FIRST_BLOCK_MAINNET;
use burnchains::bitcoin::indexer::FIRST_BLOCK_TESTNET as BITCOIN_FIRST_BLOCK_TESTNET;
use burnchains::bitcoin::indexer::FIRST_BLOCK_REGTEST as BITCOIN_FIRST_BLOCK_REGTEST;

impl BurnchainStateTransitionOps {
    pub fn noop() -> BurnchainStateTransitionOps {
        BurnchainStateTransitionOps {
            accepted_ops: vec![],
            consumed_leader_keys: vec![]
        }
    }
    pub fn from(o: BurnchainStateTransition) -> BurnchainStateTransitionOps {
        BurnchainStateTransitionOps {
            accepted_ops: o.accepted_ops,
            consumed_leader_keys: o.consumed_leader_keys
        }
    }
}

impl BurnchainStateTransition {
    pub fn noop() -> BurnchainStateTransition {
        BurnchainStateTransition {
            burn_dist: vec![],
            accepted_ops: vec![],
            consumed_leader_keys: vec![],
            
        }
    }

    pub fn from_block_ops(ic: &SortitionHandleConn, parent_snapshot: &BlockSnapshot, block_ops: &Vec<BlockstackOperationType>) -> Result<BurnchainStateTransition, burnchain_error> {
        // block commits and support burns discovered in this block.
        let mut block_commits: Vec<LeaderBlockCommitOp> = vec![];
        let mut user_burns: Vec<UserBurnSupportOp> = vec![];
        let mut accepted_ops = Vec::with_capacity(block_ops.len());

        assert!(Burnchain::ops_are_sorted(block_ops));
        
        // identify which user burns and block commits are consumed and which are not
        let mut all_user_burns : HashMap<Txid, UserBurnSupportOp> = HashMap::new();
        let mut all_block_commits : HashMap<Txid, LeaderBlockCommitOp> = HashMap::new();

        // accept all leader keys we found.
        // don't treat block commits and user burn supports just yet.
        for i in 0..block_ops.len() {
            match block_ops[i] {
                BlockstackOperationType::LeaderKeyRegister(_) => {
                    accepted_ops.push(block_ops[i].clone());
                },
                BlockstackOperationType::LeaderBlockCommit(ref op) => {
                    // we don't yet know which block commits are going to be accepted until we have
                    // the burn distribution, so just account for them for now.
                    all_block_commits.insert(op.txid.clone(), op.clone());
                    block_commits.push(op.clone());
                },
                BlockstackOperationType::UserBurnSupport(ref op) => {
                    // we don't know yet which user burns are going to be accepted until we have
                    // the burn distribution, so just account for them for now.
                    all_user_burns.insert(op.txid.clone(), op.clone());
                    user_burns.push(op.clone());
                }
            };
        }

        // find all VRF leader keys that were consumed by the block commits of this block 
        let consumed_leader_keys = SortitionDB::get_consumed_leader_keys(&ic, &parent_snapshot, &block_commits)?;

        // calculate the burn distribution from these operations.
        // The resulting distribution will contain the user burns that match block commits, and
        // will only contain block commits that consume one leader key (multiple block commits that
        // consume the same key will be rejected)
        let burn_dist = BurnSamplePoint::make_distribution(block_commits, consumed_leader_keys.clone(), user_burns);
       
        // find out which user burns and block commits we're going to take
        for i in 0..burn_dist.len() {
            let burn_point = &burn_dist[i];

            // taking this commit in this sample point
            accepted_ops.push(BlockstackOperationType::LeaderBlockCommit(burn_point.candidate.clone()));
            all_block_commits.remove(&burn_point.candidate.txid);

            // taking each user burn in this sample point
            for j in 0..burn_point.user_burns.len() {
                accepted_ops.push(BlockstackOperationType::UserBurnSupport(burn_point.user_burns[j].clone()));
                all_user_burns.remove(&burn_point.user_burns[j].txid);
            }
        }

        // accepted_ops contains all accepted commits and user burns now.
        // only rejected ones remain in all_user_burns and all_block_commits
        for op in all_block_commits.values() {
            warn!("REJECTED({}) block commit {} at {},{}: Committed to an already-consumed VRF key", op.block_height, &op.txid, op.block_height, op.vtxindex);
        }

        for op in all_user_burns.values() {
            warn!("REJECTED({}) user burn support {} at {},{}: No matching block commit in this block", op.block_height, &op.txid, op.block_height, op.vtxindex);
        }
        
        accepted_ops.sort_by(|ref a, ref b| a.vtxindex().partial_cmp(&b.vtxindex()).unwrap());
       
        Ok(BurnchainStateTransition {
            burn_dist,
            accepted_ops,
            consumed_leader_keys
        })
    }
}

impl BurnchainSigner {
    #[cfg(test)]
    pub fn new_p2pkh(pubk: &StacksPublicKey) -> BurnchainSigner {
        BurnchainSigner {
            hash_mode: AddressHashMode::SerializeP2PKH,
            num_sigs: 1,
            public_keys: vec![pubk.clone()]
        }
    }

    pub fn from_bitcoin_input(inp: &BitcoinTxInput) -> BurnchainSigner {
        match inp.in_type {
            BitcoinInputType::Standard => {
                if inp.num_required == 1 && inp.keys.len() == 1 {
                    BurnchainSigner {
                        hash_mode: AddressHashMode::SerializeP2PKH,
                        num_sigs: inp.num_required,
                        public_keys: inp.keys.clone()
                    }
                }
                else {
                    BurnchainSigner {
                        hash_mode: AddressHashMode::SerializeP2SH,
                        num_sigs: inp.num_required,
                        public_keys: inp.keys.clone()
                    }
                }
            },
            BitcoinInputType::SegwitP2SH => {
                if inp.num_required == 1 && inp.keys.len() == 1 {
                    BurnchainSigner {
                        hash_mode: AddressHashMode::SerializeP2WPKH,
                        num_sigs: inp.num_required,
                        public_keys: inp.keys.clone()
                    }
                }
                else {
                    BurnchainSigner {
                        hash_mode: AddressHashMode::SerializeP2WSH,
                        num_sigs: inp.num_required,
                        public_keys: inp.keys.clone()
                    }
                }
            }
        }
    }

    pub fn to_testnet_address(&self) -> String { 
        let addr_type = match &self.hash_mode {
            AddressHashMode::SerializeP2PKH | AddressHashMode::SerializeP2WPKH => BitcoinAddressType::PublicKeyHash,
            _ => BitcoinAddressType::ScriptHash,
        };
        BitcoinAddress::from_bytes(BitcoinNetworkType::Testnet,
                                   addr_type, 
                                   &self.to_address_bits()).unwrap().to_string()
    }

    pub fn to_address_bits(&self) -> Vec<u8> { 
        let h = public_keys_to_address_hash(&self.hash_mode, self.num_sigs, &self.public_keys);
        h.as_bytes().to_vec()
    }
}

impl BurnchainRecipient {
    pub fn from_bitcoin_output(o: &BitcoinTxOutput) -> BurnchainRecipient {
        let stacks_addr = StacksAddress::from_bitcoin_address(&o.address);
        BurnchainRecipient {
            address: stacks_addr,
            amount: o.units
        }
    }
}

impl BurnchainBlock {
    pub fn block_height(&self) -> u64 {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => data.block_height
        }
    }

    pub fn block_hash(&self) -> BurnchainHeaderHash {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => data.block_hash.clone()
        }
    }

    pub fn parent_block_hash(&self) -> BurnchainHeaderHash {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => data.parent_block_hash.clone()
        }
    }

    pub fn txs(&self) -> Vec<BurnchainTransaction> {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => data.txs.iter().map(|ref tx| BurnchainTransaction::Bitcoin((*tx).clone())).collect()
        }
    }

    pub fn timestamp(&self) -> u64 {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => data.timestamp
        }
    }
    
    pub fn header(&self) -> BurnchainBlockHeader {
        match *self {
            BurnchainBlock::Bitcoin(ref data) => {
                BurnchainBlockHeader {
                    block_height: data.block_height,
                    block_hash: data.block_hash.clone(),
                    parent_block_hash: data.parent_block_hash.clone(),
                    num_txs: data.txs.len() as u64,
                    timestamp: data.timestamp
                }
            }
        }
    }
}

impl Burnchain {
    pub fn new(working_dir: &str, chain_name: &str, network_name: &str) -> Result<Burnchain, burnchain_error> {
        let params = match (chain_name, network_name) {
            ("bitcoin", "mainnet") => BurnchainParameters::bitcoin_mainnet(),
            ("bitcoin", "testnet") => BurnchainParameters::bitcoin_testnet(),
            ("bitcoin", "regtest") => BurnchainParameters::bitcoin_regtest(),
            (_, _) => {
                return Err(burnchain_error::UnsupportedBurnchain);
            }
        };

        Ok(Burnchain {
            peer_version: PEER_VERSION,
            network_id: params.network_id,
            chain_name: params.chain_name.clone(),
            network_name: params.network_name.clone(),
            working_dir: working_dir.into(),
            consensus_hash_lifetime: params.consensus_hash_lifetime,
            stable_confirmations: params.stable_confirmations,
            first_block_height: params.first_block_height,
            first_block_hash: params.first_block_hash.clone(),
            pox_constants: PoxConstants::mainnet_default(),
        })
    }

    pub fn is_reward_cycle_start(&self, block_height: u64) -> bool {
        if block_height <= (self.first_block_height + 1) {
            // not a reward cycle start if we're the first block after genesis.
            false
        }
        else {
            let effective_height = block_height - self.first_block_height;
            // first block of the new reward cycle
            (effective_height % (self.pox_constants.reward_cycle_length as u64)) == 1
        }
    }

    #[cfg(test)]
    pub fn default_unittest(first_block_height: u64, first_block_hash: &BurnchainHeaderHash) -> Burnchain {
        let mut ret = Burnchain::new(&"/unit-tests".to_string(), &"bitcoin".to_string(), &"mainnet".to_string()).unwrap();
        ret.first_block_height = first_block_height;
        ret.first_block_hash = first_block_hash.clone();
        ret
    }

    pub fn get_chainstate_path(working_dir: &String, chain_name: &String, network_name: &String) -> String {
        let mut chainstate_dir_path = PathBuf::from(working_dir);
        chainstate_dir_path.push(chain_name);
        chainstate_dir_path.push(network_name);
        let dirpath = chainstate_dir_path.to_str().unwrap().to_string();
        dirpath
    }

    pub fn get_chainstate_config_path(working_dir: &String, chain_name: &String, network_name: &String) -> String {
        let chainstate_dir = Burnchain::get_chainstate_path(working_dir, chain_name, network_name);
        let mut config_pathbuf = PathBuf::from(&chainstate_dir);
        let chainstate_config_name = format!("{}.ini", chain_name);
        config_pathbuf.push(&chainstate_config_name);

        config_pathbuf.to_str().unwrap().to_string()
    }

    pub fn setup_chainstate_dirs(working_dir: &String, chain_name: &String, network_name: &String) -> Result<(), burnchain_error> {
        let chainstate_dir = Burnchain::get_chainstate_path(working_dir, chain_name, network_name);
        let chainstate_pathbuf = PathBuf::from(&chainstate_dir);

        if !chainstate_pathbuf.exists() {
            fs::create_dir_all(&chainstate_pathbuf)
                .map_err(burnchain_error::FSError)?;
        }
        Ok(())
    }

    pub fn make_indexer<I: BurnchainIndexer>(&self) -> Result<I, burnchain_error> {
        Burnchain::setup_chainstate_dirs(&self.working_dir, &self.chain_name, &self.network_name)?;

        let indexer : I = BurnchainIndexer::init(&self.working_dir, &self.network_name)?;
        Ok(indexer)
    }

    fn setup_chainstate<I: BurnchainIndexer>(&self, indexer: &mut I) -> Result<(), burnchain_error> {
        let headers_path = indexer.get_headers_path();
        let headers_pathbuf = PathBuf::from(&headers_path);

        let headers_height =
            if headers_pathbuf.exists() {
                indexer.get_headers_height()?
            }
            else {
                0
            };

        if !headers_pathbuf.exists() || headers_height < indexer.get_first_block_height() {
            debug!("Fetch initial headers");
            indexer.sync_headers(headers_height, None)
                .map_err(|e| {
                    error!("Failed to sync initial headers");
                    e
                })?;
        }
        Ok(())
    }

    pub fn get_db_path(&self) -> String {
        let chainstate_dir = Burnchain::get_chainstate_path(&self.working_dir, &self.chain_name, &self.network_name);
        let mut db_pathbuf = PathBuf::from(&chainstate_dir);
        db_pathbuf.push("sortition.db");
        
        let db_path = db_pathbuf.to_str().unwrap().to_string();
        db_path
    }

    pub fn get_burnchaindb_path(&self) -> String {
        let chainstate_dir = Burnchain::get_chainstate_path(&self.working_dir, &self.chain_name, &self.network_name);
        let mut db_pathbuf = PathBuf::from(&chainstate_dir);
        db_pathbuf.push("burnchain.db");
        
        let db_path = db_pathbuf.to_str().unwrap().to_string();
        db_path
    }

    pub fn connect_db<I: BurnchainIndexer>(&self, indexer: &I, readwrite: bool) -> Result<(SortitionDB, BurnchainDB), burnchain_error> {
        Burnchain::setup_chainstate_dirs(&self.working_dir, &self.chain_name, &self.network_name)?;

        let first_block_height = indexer.get_first_block_height();
        let first_block_header_hash = indexer.get_first_block_header_hash()?;
        let first_block_header_timestamp = indexer.get_first_block_header_timestamp()?;
        
        let db_path = self.get_db_path();
        let burnchain_db_path = self.get_burnchaindb_path();

        let sortitiondb = SortitionDB::connect(&db_path, first_block_height, &first_block_header_hash, first_block_header_timestamp, readwrite)?;
        let burnchaindb = BurnchainDB::connect(&burnchain_db_path, first_block_height, &first_block_header_hash, first_block_header_timestamp, readwrite)?;

        Ok((sortitiondb, burnchaindb))
    }

    /// Open the burn database.  It must already exist.
    pub fn open_db(&self, readwrite: bool) -> Result<(SortitionDB, BurnchainDB), burnchain_error> {
        let db_path = self.get_db_path();
        let burnchain_db_path = self.get_burnchaindb_path();

        let db_pathbuf = PathBuf::from(db_path.clone());
        if !db_pathbuf.exists() {
            return Err(burnchain_error::DBError(db_error::NoDBError));
        }

        let db_pathbuf = PathBuf::from(burnchain_db_path.clone());
        if !db_pathbuf.exists() {
            return Err(burnchain_error::DBError(db_error::NoDBError));
        }

        let sortition_db = SortitionDB::open(&db_path, readwrite)?;
        let burnchain_db = BurnchainDB::open(&burnchain_db_path, readwrite)?;

        Ok((sortition_db, burnchain_db))
    }

    /// Try to parse a burnchain transaction into a Blockstack operation
    pub fn classify_transaction(block_header: &BurnchainBlockHeader, burn_tx: &BurnchainTransaction) -> Option<BlockstackOperationType> {
        match burn_tx.opcode() {
            x if x == Opcodes::LeaderKeyRegister as u8 => {
                match LeaderKeyRegisterOp::from_tx(block_header, burn_tx) {
                    Ok(op) => {
                        Some(BlockstackOperationType::LeaderKeyRegister(op))
                    },
                    Err(e) => {
                        warn!("Failed to parse leader key register tx {} data {}: {:?}", &burn_tx.txid(), &to_hex(&burn_tx.data()[..]), e);
                        None
                    }
                }
            },
            x if x == Opcodes::LeaderBlockCommit as u8 => {
                match LeaderBlockCommitOp::from_tx(block_header, burn_tx) {
                    Ok(op) => {
                        Some(BlockstackOperationType::LeaderBlockCommit(op))
                    },
                    Err(e) => {
                        warn!("Failed to parse leader block commit tx {} data {}: {:?}", &burn_tx.txid(), &to_hex(&burn_tx.data()[..]), e);
                        None
                    }
                }
            },
            x if x == Opcodes::UserBurnSupport as u8 => {
                match UserBurnSupportOp::from_tx(block_header, burn_tx) {
                    Ok(op) => {
                        Some(BlockstackOperationType::UserBurnSupport(op))
                    },
                    Err(e) => {
                        warn!("Failed to parse user burn support tx {} data {}: {:?}", &burn_tx.txid(), &to_hex(&burn_tx.data()[..]), e);
                        None
                    }
                }
            },
            _ => {
                None
            }
        }
    }
   

    /// Sanity check -- a list of checked ops is sorted and all vtxindexes are unique
    pub fn ops_are_sorted(ops: &Vec<BlockstackOperationType>) -> bool {
        if ops.len() > 1 {
            for i in 0..ops.len() - 1 {
                if ops[i].vtxindex() >= ops[i+1].vtxindex() {
                    return false;
                }
            }
        }
        true
    }

    /// Verify that there are no duplicate VRF keys registered.
    /// If a key was registered more than once, take the first one and drop the rest.
    /// checked_ops must be sorted by vtxindex
    /// Returns the filtered list of blockstack ops
    pub fn filter_block_VRF_dups(mut checked_ops: Vec<BlockstackOperationType>) -> Vec<BlockstackOperationType> {
        debug!("Check Blockstack transactions: reject duplicate VRF keys");
        assert!(Burnchain::ops_are_sorted(&checked_ops));

        let mut all_keys : HashSet<VRFPublicKey> = HashSet::new();
        checked_ops.retain(|op| {
            if let BlockstackOperationType::LeaderKeyRegister(data) = op {
                if all_keys.contains(&data.public_key) {
                    // duplicate
                    warn!("REJECTED({}) leader key register {} at {},{}: Duplicate VRF key", data.block_height, &data.txid, data.block_height, data.vtxindex);
                    false
                } else {
                    // first case
                    all_keys.insert(data.public_key.clone());
                    true
                }
            } else {
                // preserve
                true
            }
        });

        checked_ops
    }

    /// Verify that two or more block commits do not consume the same VRF key.
    /// If a key is consumed more than once, then pick the block-commit with the highest burn (to
    /// stop griefing attacks).  In case of ties, pick the block-commit that occurs earlier in the
    /// block.
    pub fn filter_block_commits_with_same_VRF_key(checked_ops: Vec<BlockstackOperationType>) -> Vec<BlockstackOperationType> {
        debug!("Check Blockstack transactions: filter commits that consume the same VRF key");
        assert!(Burnchain::ops_are_sorted(&checked_ops));

        let mut ret = Vec::with_capacity(checked_ops.len());

        let mut collisions : HashMap<(u64, u32), BlockstackOperationType> = HashMap::new();
        for op in checked_ops.into_iter() {
            match op {
                BlockstackOperationType::LeaderBlockCommit(ref new_block_commit) => {
                    let key_loc = (new_block_commit.key_block_ptr as u64, new_block_commit.key_vtxindex as u32);
                    if let Some(existing_block_commit) = collisions.get_mut(&key_loc) {
                        if let BlockstackOperationType::LeaderBlockCommit(existing_block_commit) = existing_block_commit {
                            warn!("Block commit {} consumes the same VRF key as {}", &new_block_commit.block_header_hash, &existing_block_commit.block_header_hash);
                            if new_block_commit.burn_fee > existing_block_commit.burn_fee {
                                warn!("REJECTED({}) block-commit {} for {}: later competing commit {} for {} has a higher burn",
                                      existing_block_commit.block_height, &existing_block_commit.txid, &existing_block_commit.block_header_hash, &new_block_commit.txid, &new_block_commit.block_header_hash);
                                collisions.insert(key_loc, op);
                            }
                            else {
                                warn!("REJECTED({}) block-commit {} for {}: keeping earlier commit {} for {} which has a higher burn",
                                      new_block_commit.block_height, &new_block_commit.txid, &new_block_commit.block_header_hash,
                                      &existing_block_commit.txid, &existing_block_commit.block_header_hash);
                            }
                        }
                        else {
                            unreachable!("Inserted non-block-commit");
                        }
                    }
                    else {
                        collisions.insert(key_loc, op);
                    }
                },
                _ => {
                    // preserve
                    ret.push(op);
                }
            }
        }

        // fold back in 
        for (_, op) in collisions.into_iter() {
            ret.push(op);
        }

        // preserve block order
        ret.sort_by(|ref a, ref b| a.vtxindex().partial_cmp(&b.vtxindex()).unwrap());
        ret
    }

    /// Top-level entry point to check and process a block.
    pub fn process_block(burnchain_db: &mut BurnchainDB, block: &BurnchainBlock) -> Result<BurnchainBlockHeader, burnchain_error> {
        debug!("Process block {} {}", block.block_height(), &block.block_hash());

        let _blockstack_txs = burnchain_db.store_new_burnchain_block(&block)?;

        let header = block.header();

        Ok(header)
    }

    /// Hand off the block to the ChainsCoordinator _and_ process the sortition
    ///   *only* to be used by legacy stacks node interfaces, like the Helium node
    pub fn process_block_and_sortition_deprecated(db: &mut SortitionDB, burnchain_db: &mut BurnchainDB, burnchain: &Burnchain, block: &BurnchainBlock) -> Result<(BlockSnapshot, BurnchainStateTransition), burnchain_error> {
        debug!("Process block {} {}", block.block_height(), &block.block_hash());

        let header = block.header();
        let blockstack_txs = burnchain_db.store_new_burnchain_block(&block)?;

        let sortition_tip = SortitionDB::get_canonical_sortition_tip(db.conn())?;

        db.evaluate_sortition(&header, blockstack_txs, burnchain, &sortition_tip, None)
    }

    /// Determine if there has been a chain reorg, given our current canonical burnchain tip.
    /// Return the new chain tip
    fn sync_reorg<I: BurnchainIndexer>(indexer: &mut I) -> Result<u64, burnchain_error> {
        let headers_path = indexer.get_headers_path();
        
        // sanity check -- how many headers do we have? 
        // Note that this value is 1-indexed -- the smallest possible value it returns is 1,
        // So, subtract 1.
        let headers_height = indexer.get_headers_height()
            .map_err(|e| {
                error!("Failed to read headers height from {}: {:?}", headers_path, &e);
                e
            })? - 1;

        // did we encounter a reorg since last sync?  Find the highest common ancestor of the
        // remote bitcoin peer's chain state.
        // Note that this value is 0-indexed -- the smallest possible value it returns is 0.
        let reorg_height = indexer.find_chain_reorg()
            .map_err(|e| {
                error!("Failed to check for reorgs from {}: {:?}", headers_path, &e);
                e
            })?;

        if reorg_height < headers_height {
            warn!("Burnchain reorg detected: highest common ancestor at height {}", reorg_height);
            return Ok(reorg_height);
        }
        else {
            // no reorg
            return Ok(headers_height);
        }
    }

    /// Top-level burnchain sync.
    /// Returns new latest block height.
    pub fn sync<I: BurnchainIndexer + 'static>(&mut self, comms: &CoordinatorChannels) -> Result<u64, burnchain_error> {
        let mut indexer: I = self.make_indexer()?;
        let chain_tip = self.sync_with_indexer(&mut indexer, comms.clone())?;
        Ok(chain_tip.block_height)
    }

    /// Deprecated top-level burnchain sync.
    /// Returns (snapshot of new burnchain tip, last state-transition processed if any)
    /// If this method returns Err(burnchain_error::TrySyncAgain), then call this method again.
    pub fn sync_with_indexer_deprecated<I: BurnchainIndexer + 'static>(&mut self, indexer: &mut I) -> Result<(BlockSnapshot, Option<BurnchainStateTransition>), burnchain_error> {
        self.setup_chainstate(indexer)?;
        let (mut sortdb, mut burnchain_db) = self.connect_db(indexer, true)?;
        let burn_chain_tip = burnchain_db.get_canonical_chain_tip()
            .map_err(|e| {
                error!("Failed to query burn chain tip from burn DB: {}", e);
                e
            })?;

        let last_snapshot_processed = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn())?;

        // does the bunchain db have more blocks than the sortition db has processed?
        assert_eq!(last_snapshot_processed.block_height,
                   burn_chain_tip.block_height,
                   "FATAL: Last snapshot processed height={} and current burnchain db height={} have diverged",
                   last_snapshot_processed.block_height,
                   burn_chain_tip.block_height);

        let db_height = burn_chain_tip.block_height;

        // handle reorgs
        let orig_header_height = indexer.get_headers_height()?;     // 1-indexed
        let sync_height = Burnchain::sync_reorg(indexer)?;
        if sync_height + 1 < orig_header_height {
            // a reorg happened
            warn!("Dropping headers higher than {} due to burnchain reorg", sync_height);
            indexer.drop_headers(sync_height)?;
        }

        // get latest headers.
        debug!("Sync headers from {}", sync_height);

        let end_block = indexer.sync_headers(sync_height, None)?;
        let mut start_block = match sync_height {
            0 => 0,
            _ => sync_height,
        };
        if db_height < start_block {
            start_block = db_height;
        }
        
        debug!("Sync'ed headers from {} to {}. DB at {}", start_block, end_block, db_height);
        if start_block == db_height && db_height == end_block {
            // all caught up
            return Ok((last_snapshot_processed, None));
        }

        info!("Node will fetch burnchain blocks {}-{}...", start_block, end_block);

        // synchronize 
        let (downloader_send, downloader_recv) = sync_channel(1);
        let (parser_send, parser_recv) = sync_channel(1);
        let (db_send, db_recv) = sync_channel(1);

        let mut downloader = indexer.downloader();
        let mut parser = indexer.parser();

        let burnchain_config = self.clone();

        // TODO: don't re-process blocks.  See if the block hash is already present in the burn db,
        // and if so, do nothing.
        let download_thread : thread::JoinHandle<Result<(), burnchain_error>> = thread::spawn(move || {
            while let Ok(Some(ipc_header)) = downloader_recv.recv() {
                debug!("Try recv next header");

                let download_start = get_epoch_time_ms();
                let ipc_block = downloader.download(&ipc_header)?;
                let download_end = get_epoch_time_ms();

                debug!("Downloaded block {} in {}ms", ipc_block.height(), download_end.saturating_sub(download_start));

                parser_send.send(Some(ipc_block))
                    .map_err(|_e| burnchain_error::ThreadChannelError)?;
            }
            parser_send.send(None)
                .map_err(|_e| burnchain_error::ThreadChannelError)?;
            Ok(())
        });

        let parse_thread : thread::JoinHandle<Result<(), burnchain_error>> = thread::spawn(move || {
            while let Ok(Some(ipc_block)) = parser_recv.recv() {
                debug!("Try recv next block");

                let parse_start = get_epoch_time_ms();
                let burnchain_block = parser.parse(&ipc_block)?;
                let parse_end = get_epoch_time_ms();

                debug!("Parsed block {} in {}ms", burnchain_block.block_height(), parse_end.saturating_sub(parse_start));

                db_send.send(Some(burnchain_block))
                    .map_err(|_e| burnchain_error::ThreadChannelError)?;
            }
            db_send.send(None)
                .map_err(|_e| burnchain_error::ThreadChannelError)?;
            Ok(())
        });

        let db_thread : thread::JoinHandle<Result<(BlockSnapshot, Option<BurnchainStateTransition>), burnchain_error>> = thread::spawn(move || {
            let mut last_processed = (last_snapshot_processed, None);
            while let Ok(Some(burnchain_block)) = db_recv.recv() {
                debug!("Try recv next parsed block");

                if burnchain_block.block_height() == 0 {
                    continue;
                }
                
                let insert_start = get_epoch_time_ms();
                let (tip, transition) = Burnchain::process_block_and_sortition_deprecated(
                    &mut sortdb, &mut burnchain_db, &burnchain_config, &burnchain_block)?;
                last_processed = (tip, Some(transition));
                let insert_end = get_epoch_time_ms();

                debug!("Inserted block {} in {}ms", burnchain_block.block_height(), insert_end.saturating_sub(insert_start));
            }
            Ok(last_processed)
        });

        // feed the pipeline!
        let input_headers = indexer.read_headers(start_block + 1, end_block + 1)?;
        let mut downloader_result : Result<(), burnchain_error> = Ok(());
        for i in 0..input_headers.len() {
            debug!("Downloading burnchain block {} out of {}...", start_block + 1 + (i as u64), end_block);
            if let Err(e) = downloader_send.send(Some(input_headers[i].clone())) {
                info!("Failed to feed burnchain block header {}: {:?}", start_block + 1 + (i as u64), &e);
                downloader_result = Err(burnchain_error::TrySyncAgain);
                break;
            }
        }

        if downloader_result.is_ok() {
            if let Err(e) = downloader_send.send(None) {
                info!("Failed to instruct downloader thread to finish: {:?}", &e);
                downloader_result = Err(burnchain_error::TrySyncAgain);
            }
        }

        // join up 
        let _ = download_thread.join().unwrap();
        let _ = parse_thread.join().unwrap();
        let (block_snapshot, state_transition_opt) = match db_thread.join().unwrap() {
            Ok(x) => x,
            Err(e) => {
                warn!("Failed to join burnchain download thread: {:?}", &e);
                return Err(burnchain_error::TrySyncAgain);
            }
        };

        if block_snapshot.block_height < end_block {
            warn!("Try synchronizing the burn chain again: final snapshot {} < {}", block_snapshot.block_height, end_block);
            return Err(burnchain_error::TrySyncAgain);
        }

        if let Err(e) = downloader_result {
            return Err(e);
        }

        Ok((block_snapshot, state_transition_opt))
    }

    /// Top-level burnchain sync.
    /// Returns the burnchain block header for the new burnchain tip
    /// If this method returns Err(burnchain_error::TrySyncAgain), then call this method again.
    pub fn sync_with_indexer<I>(&mut self, indexer: &mut I, coord_comm: CoordinatorChannels) -> Result<BurnchainBlockHeader, burnchain_error>
    where I: BurnchainIndexer + 'static {

        self.setup_chainstate(indexer)?;
        let (_, mut burnchain_db) = self.connect_db(indexer, true)?;
        let burn_chain_tip = burnchain_db.get_canonical_chain_tip()
            .map_err(|e| {
                error!("Failed to query burn chain tip from burn DB: {}", e);
                e
            })?;

        let db_height = burn_chain_tip.block_height;

        // handle reorgs
        let orig_header_height = indexer.get_headers_height()?;     // 1-indexed
        let sync_height = Burnchain::sync_reorg(indexer)?;
        if sync_height + 1 < orig_header_height {
            // a reorg happened
            warn!("Dropping headers higher than {} due to burnchain reorg", sync_height);
            indexer.drop_headers(sync_height)?;
        }

        // get latest headers.
        debug!("Sync headers from {}", sync_height);

        let end_block = indexer.sync_headers(sync_height, None)?;
        let mut start_block = match sync_height {
            0 => 0,
            _ => sync_height,
        };
        if db_height < start_block {
            start_block = db_height;
        }
        
        debug!("Sync'ed headers from {} to {}. DB at {}", start_block, end_block, db_height);
        if start_block == db_height && db_height == end_block {
            // all caught up
            return Ok(burn_chain_tip);
        }

        info!("Node will fetch burnchain blocks {}-{}...", start_block, end_block);

        // synchronize 
        let (downloader_send, downloader_recv) = sync_channel(1);
        let (parser_send, parser_recv) = sync_channel(1);
        let (db_send, db_recv) = sync_channel(1);

        let mut downloader = indexer.downloader();
        let mut parser = indexer.parser();

        // TODO: don't re-process blocks.  See if the block hash is already present in the burn db,
        // and if so, do nothing.
        let download_thread : thread::JoinHandle<Result<(), burnchain_error>> = thread::spawn(move || {
            while let Ok(Some(ipc_header)) = downloader_recv.recv() {
                debug!("Try recv next header");

                let download_start = get_epoch_time_ms();
                let ipc_block = downloader.download(&ipc_header)?;
                let download_end = get_epoch_time_ms();

                debug!("Downloaded block {} in {}ms", ipc_block.height(), download_end.saturating_sub(download_start));

                parser_send.send(Some(ipc_block))
                    .map_err(|_e| burnchain_error::ThreadChannelError)?;
            }
            parser_send.send(None)
                .map_err(|_e| burnchain_error::ThreadChannelError)?;
            Ok(())
        });

        let parse_thread : thread::JoinHandle<Result<(), burnchain_error>> = thread::spawn(move || {
            while let Ok(Some(ipc_block)) = parser_recv.recv() {
                debug!("Try recv next block");

                let parse_start = get_epoch_time_ms();
                let burnchain_block = parser.parse(&ipc_block)?;
                let parse_end = get_epoch_time_ms();

                debug!("Parsed block {} in {}ms", burnchain_block.block_height(), parse_end.saturating_sub(parse_start));

                db_send.send(Some(burnchain_block))
                    .map_err(|_e| burnchain_error::ThreadChannelError)?;
            }
            db_send.send(None)
                .map_err(|_e| burnchain_error::ThreadChannelError)?;
            Ok(())
        });

        let db_thread : thread::JoinHandle<Result<BurnchainBlockHeader, burnchain_error>> = thread::spawn(move || {
            let mut last_processed = burn_chain_tip;
            while let Ok(Some(burnchain_block)) = db_recv.recv() {
                debug!("Try recv next parsed block");

                if burnchain_block.block_height() == 0 {
                    continue;
                }
                
                let insert_start = get_epoch_time_ms();
                last_processed = Burnchain::process_block(&mut burnchain_db, &burnchain_block)?;
                if !coord_comm.announce_new_burn_block() {
                    return Err(burnchain_error::CoordinatorClosed);
                }
                let insert_end = get_epoch_time_ms();

                debug!("Inserted block {} in {}ms", burnchain_block.block_height(), insert_end.saturating_sub(insert_start));
            }
            Ok(last_processed)
        });

        // feed the pipeline!
        let input_headers = indexer.read_headers(start_block + 1, end_block + 1)?;
        let mut downloader_result : Result<(), burnchain_error> = Ok(());
        for i in 0..input_headers.len() {
            debug!("Downloading burnchain block {} out of {}...", start_block + 1 + (i as u64), end_block);
            if let Err(e) = downloader_send.send(Some(input_headers[i].clone())) {
                info!("Failed to feed burnchain block header {}: {:?}", start_block + 1 + (i as u64), &e);
                downloader_result = Err(burnchain_error::TrySyncAgain);
                break;
            }
        }

        if downloader_result.is_ok() {
            if let Err(e) = downloader_send.send(None) {
                info!("Failed to instruct downloader thread to finish: {:?}", &e);
                downloader_result = Err(burnchain_error::TrySyncAgain);
            }
        }

        // join up 
        let _ = download_thread.join().unwrap();
        let _ = parse_thread.join().unwrap();
        let block_header = match db_thread.join().unwrap() {
            Ok(x) => x,
            Err(e) => {
                warn!("Failed to join burnchain download thread: {:?}", &e);
                if let burnchain_error::CoordinatorClosed = e {
                    return Err(burnchain_error::CoordinatorClosed)
                } else {
                    return Err(burnchain_error::TrySyncAgain)
                }
            }
        };

        if block_header.block_height < end_block {
            warn!("Try synchronizing the burn chain again: final snapshot {} < {}", block_header.block_height, end_block);
            return Err(burnchain_error::TrySyncAgain);
        }

        if let Err(e) = downloader_result {
            return Err(e);
        }

        Ok(block_header)
    }
}

#[cfg(test)]
pub mod tests {

    use burnchains::{Txid, BurnchainHeaderHash};
    use chainstate::burn::{ConsensusHash, OpsHash, BlockSnapshot, SortitionHash, VRFSeed, BlockHeaderHash};

    use chainstate::burn::db::sortdb::{
        SortitionHandleTx, SortitionDB, SortitionId, PoxId
    };

    use burnchains::*;
    use burnchains::bitcoin::keys::BitcoinPublicKey;
    use burnchains::bitcoin::address::*;
    use burnchains::bitcoin::*;

    use util::hash::hex_bytes;
    use util::log;
    use util::get_epoch_time_secs;

    use chainstate::burn::operations::{
        LeaderBlockCommitOp,
        LeaderKeyRegisterOp,
        UserBurnSupportOp,
        BlockstackOperation,
        BlockstackOperationType,
    };

    use chainstate::burn::distribution::BurnSamplePoint;

    use util::vrf::VRFPublicKey;
    use util::vrf::VRFPrivateKey;
    use ed25519_dalek::Keypair as VRFKeypair;
     
    use sha2::Sha512;

    use rand::thread_rng;
    use rand::rngs::ThreadRng;

    use util::hash::Hash160;
    use util::hash::to_hex;
    use util::uint::Uint256;
    use util::uint::Uint512;
    use util::uint::BitArray;
    use util::secp256k1::Secp256k1PrivateKey;
    use util::db::Error as db_error;

    use chainstate::stacks::StacksAddress;
    use chainstate::stacks::StacksPublicKey;

    use address::AddressHashMode;
    
    use serde::Serialize;

    use chainstate::stacks::index::TrieHash;

    #[test]
    fn test_process_block_ops() {
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000123").unwrap();
        let first_block_height = 120;
        
        let burnchain = Burnchain {
            pox_constants: PoxConstants::test_default(),
            peer_version: 0x012345678,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: first_block_height,
            first_block_hash: first_burn_hash.clone(),
        };
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000123").unwrap();        
        let block_121_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000012").unwrap();
        let block_122_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000002").unwrap();
        let block_123_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000001").unwrap();
        let block_124_hash_initial = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap();
        
        let leader_key_1 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a914306231b2782b5f80d944bf69f9d46a1453a0a0eb88ac").unwrap()).unwrap()),

            txid: Txid::from_bytes(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
            vtxindex: 456,
            block_height: 123,
            burn_header_hash: block_123_hash.clone(),
        };
        
        let leader_key_2 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a914306231b2782b5f80d944bf69f9d46a1453a0a0eb88ac").unwrap()).unwrap()),

            txid: Txid::from_bytes(&hex_bytes("9410df84e2b440055c33acb075a0687752df63fe8fe84aeec61abe469f0448c7").unwrap()).unwrap(),
            vtxindex: 457,
            block_height: 122,
            burn_header_hash: block_122_hash.clone(),
        };

        let leader_key_3 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("de8af7037e522e65d2fe2d63fb1b764bfea829df78b84444338379df13144a02").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a914f464a593895cd58c74a7352dd4a65c491d0c0bf688ac").unwrap()).unwrap()),

            txid: Txid::from_bytes(&hex_bytes("eb54704f71d4a2d1128d60ffccced547054b52250ada6f3e7356165714f44d4c").unwrap()).unwrap(),
            vtxindex: 10,
            block_height: 121,
            burn_header_hash: block_121_hash.clone(),
        };
        
        let user_burn_1 = UserBurnSupportOp {
            address: StacksAddress::new(1, Hash160([1u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("7150f635054b87df566a970b21e07030d6444bf2").unwrap()).unwrap(),       // 22222....2222
            key_block_ptr: 123,
            key_vtxindex: 456,
            burn_fee: 10000,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716b").unwrap()).unwrap(),
            vtxindex: 13,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };

        let user_burn_1_2 = UserBurnSupportOp {
            address: StacksAddress::new(2, Hash160([2u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("7150f635054b87df566a970b21e07030d6444bf2").unwrap()).unwrap(),       // 22222....2222
            key_block_ptr: 123,
            key_vtxindex: 456,
            burn_fee: 30000,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716c").unwrap()).unwrap(),
            vtxindex: 14,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };

        let user_burn_2 = UserBurnSupportOp {
            address: StacksAddress::new(3, Hash160([3u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),       // 22222....2223
            key_block_ptr: 122,
            key_vtxindex: 457,
            burn_fee: 20000,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716d").unwrap()).unwrap(),
            vtxindex: 15,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };
        
        let user_burn_2_2 = UserBurnSupportOp {
            address: StacksAddress::new(4, Hash160([4u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),       // 22222....2223
            key_block_ptr: 122,
            key_vtxindex: 457,
            burn_fee: 40000,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716e").unwrap()).unwrap(),
            vtxindex: 16,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };
       
        // should be rejected
        let user_burn_noblock = UserBurnSupportOp {
            address: StacksAddress::new(5, Hash160([5u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("3333333333333333333333333333333333333333").unwrap()).unwrap(),
            key_block_ptr: 122,
            key_vtxindex: 772,
            burn_fee: 12345,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c716f").unwrap()).unwrap(),
            vtxindex: 12,
            block_height: 123,
            burn_header_hash: block_123_hash.clone(),
        };
        
        // should be rejected
        let user_burn_nokey = UserBurnSupportOp {
            address: StacksAddress::new(6, Hash160([6u8; 20])),
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("3f3338db51f2b1f6ac0cf6177179a24ee130c04ef2f9849a64a216969ab60e70").unwrap()).unwrap(),
            block_header_hash_160: Hash160::from_bytes(&hex_bytes("037a1e860899a4fa823c18b66f6264d20236ec58").unwrap()).unwrap(),
            key_block_ptr: 122,
            key_vtxindex: 457,
            burn_fee: 12345,

            txid: Txid::from_bytes(&hex_bytes("1d5cbdd276495b07f0e0bf0181fa57c175b217bc35531b078d62fc20986c7170").unwrap()).unwrap(),
            vtxindex: 15,
            block_height: 123,
            burn_header_hash: block_123_hash.clone(),
        };

        let block_commit_1 = LeaderBlockCommitOp {
            commit_outs: vec![],
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
            parent_block_ptr: 0,
            parent_vtxindex: 0,
            key_block_ptr: 123,
            key_vtxindex: 456,
            memo: vec![0x80],

            burn_fee: 12345,
            input: BurnchainSigner {
                public_keys: vec![
                    StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_sigs: 1,
                hash_mode: AddressHashMode::SerializeP2PKH
            },

            txid: Txid::from_bytes(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
            vtxindex: 444,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };

        let block_commit_2 = LeaderBlockCommitOp {
            commit_outs: vec![],
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222223").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333334").unwrap()).unwrap(),
            parent_block_ptr: 0,
            parent_vtxindex: 0,
            key_block_ptr: 122,
            key_vtxindex: 457,
            memo: vec![0x80],

            burn_fee: 12345,
            input: BurnchainSigner {
                public_keys: vec![
                    StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_sigs: 1,
                hash_mode: AddressHashMode::SerializeP2PKH
            },

            txid: Txid::from_bytes(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27d0").unwrap()).unwrap(),
            vtxindex: 445,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };        
        
        let block_commit_3 = LeaderBlockCommitOp {
            commit_outs: vec![],
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222224").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333335").unwrap()).unwrap(),
            parent_block_ptr: 0,
            parent_vtxindex: 0,
            key_block_ptr: 121,
            key_vtxindex: 10,
            memo: vec![0x80],

            burn_fee: 23456,
            input: BurnchainSigner {
                public_keys: vec![
                    StacksPublicKey::from_hex("0283d603abdd2392646dbdd0dc80beb39c25bfab96a8a921ea5e7517ce533f8cd5").unwrap(),
                ],
                num_sigs: 1,
                hash_mode: AddressHashMode::SerializeP2PKH
            },

            txid: Txid::from_bytes(&hex_bytes("301dc687a9f06a1ae87a013f27133e9cec0843c2983567be73e185827c7c13de").unwrap()).unwrap(),
            vtxindex: 446,
            block_height: 124,
            burn_header_hash: block_124_hash_initial.clone(),
        };

        let block_ops_121 : Vec<BlockstackOperationType> = vec![
            BlockstackOperationType::LeaderKeyRegister(leader_key_3.clone())
        ];
        let block_opshash_121 = OpsHash::from_txids(&vec![leader_key_3.txid.clone()]);
        let block_prev_chs_121 = vec![
            ConsensusHash::from_hex("0000000000000000000000000000000000000000").unwrap(),
        ];
        let mut block_121_snapshot = BlockSnapshot {
            pox_valid: true,
            block_height: 121,
            burn_header_hash: block_121_hash.clone(),
            sortition_id: SortitionId(block_121_hash.0.clone()),
            burn_header_timestamp: 121,
            parent_burn_header_hash: first_burn_hash.clone(),
            ops_hash: block_opshash_121.clone(),
            consensus_hash: ConsensusHash::from_ops(&block_121_hash, &block_opshash_121, 0, &block_prev_chs_121, &PoxId::stubbed()),
            total_burn: 0,
            sortition: false,
            sortition_hash: SortitionHash::initial()
                .mix_burn_header(&block_121_hash),
            winning_block_txid: Txid::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            winning_stacks_block_hash: BlockHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            index_root: TrieHash::from_empty_data(),        // TBD
            num_sortitions: 0,
            stacks_block_accepted: false,
            stacks_block_height: 0,
            arrival_index: 0,
            canonical_stacks_tip_height: 0,
            canonical_stacks_tip_hash: BlockHeaderHash([0u8; 32]),
            canonical_stacks_tip_consensus_hash: ConsensusHash([0u8; 20]),
        };

        let block_ops_122 = vec![
            BlockstackOperationType::LeaderKeyRegister(leader_key_2.clone())
        ];
        let block_opshash_122 = OpsHash::from_txids(&vec![leader_key_2.txid.clone()]);
        let block_prev_chs_122 = vec![
            block_121_snapshot.consensus_hash.clone(),
            ConsensusHash::from_hex("0000000000000000000000000000000000000000").unwrap(),
        ];
        let mut block_122_snapshot = BlockSnapshot {
            pox_valid: true,
            block_height: 122,
            burn_header_hash: block_122_hash.clone(),
            sortition_id: SortitionId(block_122_hash.0.clone()),
            burn_header_timestamp: 122,
            parent_burn_header_hash: block_121_hash.clone(),
            ops_hash: block_opshash_122.clone(),
            consensus_hash: ConsensusHash::from_ops(&block_122_hash, &block_opshash_122, 0, &block_prev_chs_122, &PoxId::stubbed()),
            total_burn: 0,
            sortition: false,
            sortition_hash: SortitionHash::initial()
                .mix_burn_header(&block_121_hash)
                .mix_burn_header(&block_122_hash),
            winning_block_txid: Txid::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            winning_stacks_block_hash: BlockHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            index_root: TrieHash::from_empty_data(),        // TBD
            num_sortitions: 0,
            stacks_block_accepted: false,
            stacks_block_height: 0,
            arrival_index: 0,
            canonical_stacks_tip_height: 0,
            canonical_stacks_tip_hash: BlockHeaderHash([0u8; 32]),
            canonical_stacks_tip_consensus_hash: ConsensusHash([0u8; 20]),
        };

        let block_ops_123 = vec![
            BlockstackOperationType::UserBurnSupport(user_burn_noblock.clone()),
            BlockstackOperationType::UserBurnSupport(user_burn_nokey.clone()),
            BlockstackOperationType::LeaderKeyRegister(leader_key_1.clone()),
        ];
        let block_opshash_123 = OpsHash::from_txids(&vec![
            // notably, the user burns here _wont_ be included in the consensus hash
            leader_key_1.txid.clone(),
        ]);
        let block_prev_chs_123 = vec![
            block_122_snapshot.consensus_hash.clone(),
            block_121_snapshot.consensus_hash.clone(),
        ];
        let mut block_123_snapshot = BlockSnapshot {
            pox_valid: true,
            block_height: 123,
            burn_header_hash: block_123_hash.clone(),
            sortition_id: SortitionId(block_123_hash.0.clone()),
            burn_header_timestamp: 123,
            parent_burn_header_hash: block_122_hash.clone(),
            ops_hash: block_opshash_123.clone(),
            consensus_hash: ConsensusHash::from_ops(&block_123_hash, &block_opshash_123, 0, &block_prev_chs_123, &PoxId::stubbed()),        // user burns not included, so zero burns this block
            total_burn: 0,
            sortition: false,
            sortition_hash: SortitionHash::initial()
                .mix_burn_header(&block_121_hash)
                .mix_burn_header(&block_122_hash)
                .mix_burn_header(&block_123_hash),
            winning_block_txid: Txid::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            winning_stacks_block_hash: BlockHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
            index_root: TrieHash::from_empty_data(),        // TBD
            num_sortitions: 0,
            stacks_block_accepted: false,
            stacks_block_height: 0,
            arrival_index: 0,
            canonical_stacks_tip_height: 0,
            canonical_stacks_tip_hash: BlockHeaderHash([0u8; 32]),
            canonical_stacks_tip_consensus_hash: ConsensusHash([0u8; 20]),
        };

        // multiple possibilities for block 124 -- we'll reorg the chain each time back to 123 and
        // re-try block 124 to test them all.
        let block_ops_124_possibilities = vec![
            vec![
                BlockstackOperationType::LeaderBlockCommit(block_commit_1.clone()),
            ],
            vec![
                BlockstackOperationType::LeaderBlockCommit(block_commit_1.clone()),
                BlockstackOperationType::LeaderBlockCommit(block_commit_2.clone()),
                BlockstackOperationType::LeaderBlockCommit(block_commit_3.clone()),
            ],
            vec![
                BlockstackOperationType::UserBurnSupport(user_burn_1.clone()),
                BlockstackOperationType::UserBurnSupport(user_burn_1_2.clone()),
                BlockstackOperationType::UserBurnSupport(user_burn_2.clone()),
                BlockstackOperationType::UserBurnSupport(user_burn_2_2.clone()),
                BlockstackOperationType::LeaderBlockCommit(block_commit_1.clone()),
                BlockstackOperationType::LeaderBlockCommit(block_commit_2.clone()),
                BlockstackOperationType::LeaderBlockCommit(block_commit_3.clone())
            ],
        ];

        let block_124_winners = vec![
            block_commit_1.clone(),
            block_commit_3.clone(),
            block_commit_1.clone(),
        ];
 
        let mut db = SortitionDB::connect_test(first_block_height, &first_burn_hash).unwrap();
       
        // NOTE: the .txs() method will NOT be called, so we can pass an empty vec![] here
        let block121 = BurnchainBlock::Bitcoin(BitcoinBlock::new(121, &block_121_hash, &first_burn_hash, &vec![], 121));
        let block122 = BurnchainBlock::Bitcoin(BitcoinBlock::new(122, &block_122_hash, &block_121_hash, &vec![], 122));
        let block123 = BurnchainBlock::Bitcoin(BitcoinBlock::new(123, &block_123_hash, &block_122_hash, &vec![], 123));

        let initial_snapshot = BlockSnapshot::initial(first_block_height, &first_burn_hash, first_block_height as u64);

        // process up to 124 
        {
            let header = block121.header();
            let mut tx = SortitionHandleTx::begin(&mut db, &initial_snapshot.sortition_id).unwrap();

            let (sn121, _) = tx.process_block_ops(&burnchain, &initial_snapshot, &header, block_ops_121, None, PoxId::stubbed(), None).unwrap();
            tx.commit().unwrap();
           
            block_121_snapshot.index_root = sn121.index_root.clone();
            assert_eq!(sn121, block_121_snapshot);
        }
        {
            let header = block122.header();
            let mut tx = SortitionHandleTx::begin(&mut db, &block_121_snapshot.sortition_id).unwrap();

            let (sn122, _) = tx.process_block_ops(&burnchain, &block_121_snapshot, &header, block_ops_122, None, PoxId::stubbed(), None).unwrap();
            tx.commit().unwrap();
            
            block_122_snapshot.index_root = sn122.index_root.clone();
            assert_eq!(sn122, block_122_snapshot);
        }
        {
            let header = block123.header();
            let mut tx = SortitionHandleTx::begin(&mut db, &block_122_snapshot.sortition_id).unwrap();
            let (sn123, _) = tx.process_block_ops(&burnchain, &block_122_snapshot, &header, block_ops_123, None, PoxId::stubbed(), None).unwrap();
            tx.commit().unwrap();
            
            block_123_snapshot.index_root = sn123.index_root.clone();
            assert_eq!(sn123, block_123_snapshot);
        }

        for scenario_idx in 0..block_ops_124_possibilities.len() {
            let mut block_ops_124 = block_ops_124_possibilities[scenario_idx].clone();
            let mut block_124_hash_bytes = block_124_hash_initial.as_bytes().clone();
            block_124_hash_bytes[0] = (scenario_idx + 1) as u8;
            let block_124_hash = BurnchainHeaderHash(block_124_hash_bytes);

            for i in 0..block_ops_124.len() {
                match block_ops_124[i] {
                    BlockstackOperationType::LeaderKeyRegister(ref mut op) => {
                        op.burn_header_hash = block_124_hash.clone();
                    },
                    BlockstackOperationType::LeaderBlockCommit(ref mut op) => {
                        op.burn_header_hash = block_124_hash.clone();
                    },
                    BlockstackOperationType::UserBurnSupport(ref mut op) => {
                        op.burn_header_hash = block_124_hash.clone();
                    }
                }
            }

            // everything will be included
            let block_opshash_124 = OpsHash::from_txids(
                &block_ops_124
                .clone()
                .into_iter()
                .map(|bo| {
                    match bo {
                        BlockstackOperationType::LeaderBlockCommit(ref op) => op.txid.clone(),
                        BlockstackOperationType::LeaderKeyRegister(ref op) => op.txid.clone(),
                        BlockstackOperationType::UserBurnSupport(ref op) => op.txid.clone()
                    }
                })
                .collect()
            );
            let block_prev_chs_124 = vec![
                block_123_snapshot.consensus_hash.clone(),
                block_122_snapshot.consensus_hash.clone(),
                ConsensusHash::from_hex("0000000000000000000000000000000000000000").unwrap(),
            ];
            let burn_total = block_ops_124
                .iter()
                .fold(0u64, |mut acc, op| {
                    let bf = match op {
                        BlockstackOperationType::LeaderBlockCommit(ref op) => op.burn_fee,
                        BlockstackOperationType::UserBurnSupport(ref op) => op.burn_fee,
                        _ => 0
                    };
                    acc += bf;
                    acc
                });

            let next_sortition = block_ops_124.len() > 0 && burn_total > 0;
            
            let mut block_124_snapshot = BlockSnapshot {
                pox_valid: true,
                block_height: 124,
                burn_header_hash: block_124_hash.clone(),
                sortition_id: SortitionId(block_124_hash.0.clone()),
                burn_header_timestamp: 124,
                parent_burn_header_hash: block_123_snapshot.burn_header_hash.clone(),
                ops_hash: block_opshash_124.clone(),
                consensus_hash: ConsensusHash::from_ops(&block_124_hash, &block_opshash_124, burn_total, &block_prev_chs_124, &PoxId::stubbed()),
                total_burn: burn_total,
                sortition: next_sortition,
                sortition_hash: SortitionHash::initial()
                    .mix_burn_header(&block_121_hash)
                    .mix_burn_header(&block_122_hash)
                    .mix_burn_header(&block_123_hash)
                    .mix_burn_header(&block_124_hash),
                winning_block_txid: block_124_winners[scenario_idx].txid.clone(),
                winning_stacks_block_hash: block_124_winners[scenario_idx].block_header_hash.clone(),
                index_root: TrieHash::from_empty_data(),        // TDB
                num_sortitions: if next_sortition { 1 } else { 0 },
                stacks_block_accepted: false,
                stacks_block_height: 0,
                arrival_index: 0,
                canonical_stacks_tip_height: 0,
                canonical_stacks_tip_hash: BlockHeaderHash([0u8; 32]),
                canonical_stacks_tip_consensus_hash: ConsensusHash([0u8; 20]),
            };

            if next_sortition {
                block_124_snapshot.sortition_hash = block_124_snapshot.sortition_hash.mix_VRF_seed(&block_124_winners[scenario_idx].new_seed);
            }

            let block124 = BurnchainBlock::Bitcoin(BitcoinBlock::new(124, &block_124_hash, &block_123_hash, &vec![], 124));

            // process this scenario
            let sn124 = {
                let header = block124.header();
                let mut tx = SortitionHandleTx::begin(&mut db, &block_123_snapshot.sortition_id).unwrap();
                let (sn124, _) = tx.process_block_ops(&burnchain, &block_123_snapshot, &header, block_ops_124, None, PoxId::stubbed(), None).unwrap();
                tx.commit().unwrap();

                block_124_snapshot.index_root = sn124.index_root.clone();
                sn124
            };
           
            assert_eq!(sn124, block_124_snapshot);

            // get all winning block commit hashes.
            // There should only be two -- the winning block at height 124, and the genesis
            // sentinel block hash.  This is because epochs 121, 122, and 123 don't have any block
            // commits.
            let expected_winning_hashes = vec![
                BlockHeaderHash([0u8; 32]),
                block_124_winners[scenario_idx].block_header_hash.clone()
            ];

            // TODO: pair up with stacks chain state?
            /*
            let winning_header_hashes = {
                let mut tx = db.tx_begin().unwrap();
                BurnDB::get_stacks_block_header_inventory(&mut tx, 124).unwrap()
                    .iter()
                    .map(|ref hinv| hinv.0.clone())
                    .collect()
            };

            assert_eq!(expected_winning_hashes, winning_header_hashes);
            */
        }
    }

    #[test]
    fn test_filter_block_commits_with_same_VRF_key() {
        let mut block_commits = vec![];

        for i in 0..10 {
            let op = BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
                commit_outs: vec![],
                block_header_hash: BlockHeaderHash::from_bytes(&vec![i,i,i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
                new_seed: VRFSeed::from_bytes(&vec![i,i,i,i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
                parent_block_ptr: 3,
                parent_vtxindex: 1,
                key_block_ptr: 2,       // make them all try to use the same VRF key
                key_vtxindex: 1,
                memo: vec![i],

                burn_fee: (i + 1) as u64,
                input: BurnchainSigner {
                    public_keys: vec![
                        StacksPublicKey::from_hex("02113c274c05ed0b7f9d08f41ca674b22e42188408caaff82a350b024442de353c").unwrap(),
                    ],
                    num_sigs: 1,
                    hash_mode: AddressHashMode::SerializeP2PKH
                },

                txid: Txid::from_bytes(&vec![i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i]).unwrap(),
                vtxindex: (i + 2) as u32,
                block_height: 5,
                burn_header_hash: BurnchainHeaderHash([0xff; 32]),
            });
            block_commits.push(op);
        }

        let noncolliding_op = BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
            commit_outs: vec![],
            block_header_hash: BlockHeaderHash([0xbb; 32]),
            new_seed: VRFSeed([0xcc; 32]),
            parent_block_ptr: 3,
            parent_vtxindex: 1,
            key_block_ptr: 2,
            key_vtxindex: 2,
            memo: vec![0x00],

            burn_fee: 256,
            input: BurnchainSigner {
                public_keys: vec![
                    StacksPublicKey::from_hex("02113c274c05ed0b7f9d08f41ca674b22e42188408caaff82a350b024442de353c").unwrap(),
                ],
                num_sigs: 1,
                hash_mode: AddressHashMode::SerializeP2PKH
            },

            txid: Txid([0xdd; 32]),
            vtxindex: 1,
            block_height: 5,
            burn_header_hash: BurnchainHeaderHash([0xff; 32]),
        });
            
        let mut csprng: ThreadRng = thread_rng();
        let keypair: VRFKeypair = VRFKeypair::generate(&mut csprng);

        let key_op = BlockstackOperationType::LeaderKeyRegister(LeaderKeyRegisterOp {
            consensus_hash: ConsensusHash([0x11; 20]),
            public_key: VRFPublicKey::from_bytes(&keypair.public.to_bytes()).unwrap(),
            memo: vec![0, 0, 0, 0, 0],
            address: StacksAddress { version: 1, bytes: Hash160([0x33; 20]) },

            txid: Txid::from_bytes(&vec![4,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
            vtxindex: 100,
            block_height: 5,
            burn_header_hash: BurnchainHeaderHash([0xff; 32])
        });


        // add a non-colliding one
        block_commits.push(noncolliding_op.clone());
        block_commits.push(key_op.clone());

        let winner = block_commits[9].clone();
        
        block_commits.sort_by(|ref a, ref b| a.vtxindex().partial_cmp(&b.vtxindex()).unwrap());

        let ops = Burnchain::filter_block_commits_with_same_VRF_key(block_commits);
        assert_eq!(ops.len(), 3);
        
        // first op should be the non-colliding one
        match (ops[0].clone(), noncolliding_op) {
            (BlockstackOperationType::LeaderBlockCommit(ref op1), BlockstackOperationType::LeaderBlockCommit(ref op2)) => {
                assert_eq!(op1, op2);
            },
            (_, _) => {
                assert!(false);
            }
        }
        
        // second op should be the colliding op with the higehst fee
        match (ops[1].clone(), winner) {
            (BlockstackOperationType::LeaderBlockCommit(ref op1), BlockstackOperationType::LeaderBlockCommit(ref op2)) => {
                assert_eq!(op1, op2);
            },
            (_, _) => {
                assert!(false);
            }
        }
        
        // third op should be the leader key (untouched)
        match (ops[2].clone(), key_op) {
            (BlockstackOperationType::LeaderKeyRegister(ref op1), BlockstackOperationType::LeaderKeyRegister(ref op2)) => {
                assert_eq!(op1, op2);
            },
            (_, _) => {
                assert!(false);
            }
        }
    }

    #[test]
    fn test_burn_snapshot_sequence() {
        
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000123").unwrap();
        let first_block_height = 120;
        
        let burnchain = Burnchain {
            pox_constants: PoxConstants::test_default(),
            peer_version: 0x012345678,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: first_block_height,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut leader_private_keys = vec![];
        let mut leader_public_keys = vec![];
        let mut leader_bitcoin_public_keys = vec![];
        let mut leader_bitcoin_addresses = vec![];

        for i in 0..32 {
            let mut csprng: ThreadRng = thread_rng();
            let keypair: VRFKeypair = VRFKeypair::generate(&mut csprng);

            let privkey_hex = to_hex(&keypair.secret.to_bytes());
            leader_private_keys.push(privkey_hex);

            let pubkey_hex = to_hex(&keypair.public.to_bytes());
            leader_public_keys.push(pubkey_hex);

            let bitcoin_privkey = Secp256k1PrivateKey::new();
            let bitcoin_publickey = BitcoinPublicKey::from_private(&bitcoin_privkey);

            leader_bitcoin_public_keys.push(to_hex(&bitcoin_publickey.to_bytes()));

            let btc_input = BitcoinTxInput {
                in_type: BitcoinInputType::Standard,
                keys: vec![bitcoin_publickey.clone()],
                num_required: 1
            };

            leader_bitcoin_addresses.push(BitcoinAddress::from_bytes(BitcoinNetworkType::Testnet, BitcoinAddressType::PublicKeyHash, &btc_input.to_address_bits()).unwrap());
        }

        let mut expected_burn_total : u64 = 0;

        // insert all operations
        let mut db = SortitionDB::connect_test(first_block_height, &first_burn_hash).unwrap();
        let mut prev_snapshot = BlockSnapshot::initial(first_block_height, &first_burn_hash, first_block_height as u64);
        let mut all_stacks_block_hashes = vec![];

        for i in 0..32 {

            let mut block_ops = vec![];
            let burn_block_hash = BurnchainHeaderHash::from_bytes(&vec![i+1,i+1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i+1]).unwrap();
            let parent_burn_block_hash = prev_snapshot.burn_header_hash.clone();
            let parent_index_root = prev_snapshot.index_root.clone();
            
            // insert block commit paired to previous round's leader key, as well as a user burn
            if i > 0 {
                let next_block_commit = LeaderBlockCommitOp {
                    commit_outs: vec![],
                    block_header_hash: BlockHeaderHash::from_bytes(&vec![i,i,i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
                    new_seed: VRFSeed::from_bytes(&vec![i,i,i,i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
                    parent_block_ptr: (if i == 1 { 0 } else { first_block_height + (i as u64) }) as u32,
                    parent_vtxindex: (if i == 1 { 0 } else { 2 * (i - 1) }) as u16,
                    key_block_ptr: (first_block_height + (i as u64)) as u32,
                    key_vtxindex: (2 * (i - 1) + 1) as u16,
                    memo: vec![i],

                    burn_fee: i as u64,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex(&leader_bitcoin_public_keys[(i-1) as usize].clone()).unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes(&vec![i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i]).unwrap(),
                    vtxindex: (2 * i) as u32,
                    block_height: first_block_height + ((i + 1) as u64),
                    burn_header_hash: burn_block_hash.clone(),
                };

                all_stacks_block_hashes.push(next_block_commit.block_header_hash.clone());
                block_ops.push(BlockstackOperationType::LeaderBlockCommit(next_block_commit));
            }

            let ch = {
                let ic = db.index_handle(&prev_snapshot.sortition_id);
                ic.get_consensus_at((i as u64) + first_block_height).unwrap().unwrap_or(ConsensusHash::empty())
            };

            let next_leader_key = LeaderKeyRegisterOp {
                consensus_hash: ch.clone(),
                public_key: VRFPublicKey::from_bytes(&hex_bytes(&leader_public_keys[i as usize]).unwrap()).unwrap(),
                memo: vec![0, 0, 0, 0, i],
                address: StacksAddress::from_bitcoin_address(&leader_bitcoin_addresses[i as usize].clone()),

                txid: Txid::from_bytes(&vec![i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap(),
                vtxindex: (2 * i + 1) as u32,
                block_height: first_block_height + (i + 1) as u64,
                burn_header_hash: burn_block_hash.clone(),
            };

            block_ops.push(BlockstackOperationType::LeaderKeyRegister(next_leader_key));

            let block = BurnchainBlock::Bitcoin(BitcoinBlock::new(first_block_height + (i + 1) as u64, &burn_block_hash, &parent_burn_block_hash, &vec![], get_epoch_time_secs()));

            // process this block
            let snapshot = {
                let header = block.header();
                let mut tx = SortitionHandleTx::begin(&mut db, &prev_snapshot.sortition_id).unwrap();
                let (sn, _) = tx.process_block_ops(&burnchain, &prev_snapshot, &header, block_ops, None, PoxId::stubbed(), None).unwrap();
                tx.commit().unwrap();
                sn
            };

            if i > 0 {
                expected_burn_total += i as u64;

                assert_eq!(snapshot.total_burn, expected_burn_total);
                assert_eq!(snapshot.winning_block_txid, Txid::from_bytes(&vec![i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i]).unwrap());
                assert_eq!(snapshot.winning_stacks_block_hash, BlockHeaderHash::from_bytes(&vec![i,i,i,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]).unwrap());
                assert_eq!(snapshot.burn_header_hash, burn_block_hash);
                assert_eq!(snapshot.parent_burn_header_hash, parent_burn_block_hash);
                assert_eq!(snapshot.block_height, (i as u64) + 1 + first_block_height);
                assert!(snapshot.sortition);
            }
            else {
                assert!(!snapshot.sortition);
                assert_eq!(snapshot.total_burn, 0);
            }

            prev_snapshot = snapshot;
        }
    }

    // TODO: test VRF key duplication check
    // TODO; test that all but the first of the block commits committing to the same key are
    // dropped
    // TODO: test that we can get the histories of all Stacks block headers from different fork segments
    // TODO: test top-level sync with a burn chain reorg
    // -- make sure the chain can switch from fork A to fork B back to fork A safely.
    // TODO: test that only relevant user burns get stored in a burn distribution, and that they're
    // all present in the DB
}
