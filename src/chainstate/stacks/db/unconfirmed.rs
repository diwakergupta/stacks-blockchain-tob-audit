/*
 copyright: (c) 2013-2020 by Blockstack PBC, a public benefit corporation.

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

use std::fs;
use std::collections::HashSet;

use core::*;

use chainstate::stacks::Error;
use chainstate::stacks::*;
use chainstate::stacks::events::*;
use chainstate::stacks::db::*;
use chainstate::stacks::db::accounts::*;
use chainstate::stacks::db::blocks::*;

use util::db::Error as db_error;
use net::Error as net_error;

use vm::database::marf::MarfedKV;
use vm::database::NULL_HEADER_DB;
use vm::database::NULL_BURN_STATE_DB;
use vm::database::HeadersDB;
use vm::database::BurnStateDB;
use vm::clarity::{
    ClarityInstance,
    Error as clarity_error
};

use vm::costs::ExecutionCost;

pub struct UnconfirmedState {
    pub confirmed_chain_tip: StacksBlockId,
    pub unconfirmed_chain_tip: StacksBlockId,
    pub clarity_inst: ClarityInstance,

    last_mblock: Option<BlockHeaderHash>,
    last_mblock_seq: u16,
}

impl UnconfirmedState {
    pub fn new(chainstate: &StacksChainState, tip: StacksBlockId, cost_so_far: ExecutionCost) -> Result<UnconfirmedState, Error> {
        let marf = MarfedKV::open_unconfirmed(&chainstate.clarity_state_index_root, None)?;
        
        let mut microblock_budget = chainstate.block_limit.clone();
        microblock_budget.sub(&cost_so_far)
            .map_err(|_e| {
                Error::CostOverflowError(chainstate.block_limit.clone(), cost_so_far.clone(), cost_so_far.clone())
            })?;

        let clarity_instance = ClarityInstance::new(marf, microblock_budget);
        let unconfirmed_tip = MARF::make_unconfirmed_chain_tip(&tip);

        Ok(UnconfirmedState {
            confirmed_chain_tip: tip,
            unconfirmed_chain_tip: unconfirmed_tip,
            clarity_inst: clarity_instance,

            last_mblock: None,
            last_mblock_seq: 0,
        })
    }

    pub fn open_readonly(chainstate: &StacksChainState, tip: StacksBlockId) -> Result<UnconfirmedState, Error> {
        let marf = MarfedKV::open_unconfirmed(&chainstate.clarity_state_index_root, None)?;
        let clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let unconfirmed_tip = MARF::make_unconfirmed_chain_tip(&tip);
        
        Ok(UnconfirmedState {
            confirmed_chain_tip: tip,
            unconfirmed_chain_tip: unconfirmed_tip,
            clarity_inst: clarity_instance,

            last_mblock: None,
            last_mblock_seq: u16::max_value()
        })
    }

    /// Append a sequence of microblocks to this unconfirmed state.
    /// Microblocks with sequence less than the self.last_mblock_seq will be silently ignored.
    /// Produce the total fees, total burns, and total list of transaction receipts.
    /// Updates internal cost_so_far count.
    /// Idempotent.
    fn append_microblocks(&mut self, chainstate: &StacksChainState, burn_dbconn: &dyn BurnStateDB, mblocks: Vec<StacksMicroblock>) -> Result<(u128, u128, Vec<StacksTransactionReceipt>), Error> {
        if self.last_mblock_seq == u16::max_value() {
            // drop them
            return Ok((0, 0, vec![]));
        }

        debug!("Refresh unconfirmed chain state off of {} with {} microblocks", &self.confirmed_chain_tip, mblocks.len());

        let mut last_mblock = self.last_mblock.take();
        let mut last_mblock_seq = self.last_mblock_seq;
        let db_config = chainstate.config();

        let mut total_fees = 0;
        let mut total_burns = 0;
        let mut all_receipts = vec![];
   
        {
            let mut clarity_tx = StacksChainState::begin_unconfirmed(db_config, &chainstate.headers_db, &mut self.clarity_inst, burn_dbconn, &self.confirmed_chain_tip);

            for mblock in mblocks.into_iter() {
                if (last_mblock.is_some() && mblock.header.sequence <= last_mblock_seq) || (last_mblock.is_none() && mblock.header.sequence != 0) {
                    continue;
                }

                let seq = mblock.header.sequence;
                let mblock_hash = mblock.block_hash();

                let (stx_fees, stx_burns, mut receipts) = match StacksChainState::process_microblocks_transactions(&mut clarity_tx, &vec![mblock]) {
                    Ok(x) => x,
                    Err((Error::InvalidStacksMicroblock(msg, _), hdr)) => {
                        warn!("Invalid stacks microblock {}: {}", hdr, msg);
                        continue;
                    },
                    Err((e, _)) => {
                        return Err(e);
                    }
                };

                total_fees += stx_fees;
                total_burns += stx_burns;
                all_receipts.append(&mut receipts);

                last_mblock = Some(mblock_hash);
                last_mblock_seq = seq;
            }

            clarity_tx.commit_unconfirmed();
        };

        self.last_mblock = last_mblock;
        self.last_mblock_seq = last_mblock_seq;

        Ok((total_fees, total_burns, all_receipts))
    }

    /// Load up Stacks microblock stream to process
    fn load_child_microblocks(&self, chainstate: &StacksChainState) -> Result<Option<Vec<StacksMicroblock>>, Error> {
        let (consensus_hash, anchored_block_hash) = match chainstate.get_block_header_hashes(&self.confirmed_chain_tip)? {
            Some(x) => x,
            None => {
                return Err(Error::NoSuchBlockError);
            }
        };
        
        StacksChainState::load_staging_microblock_stream(&chainstate.blocks_db, &chainstate.blocks_path, &consensus_hash, &anchored_block_hash, u16::max_value())
    }

    /// Update the view of the current confiremd chain tip's unconfirmed microblock state
    pub fn refresh(&mut self, chainstate: &StacksChainState, burn_dbconn: &dyn BurnStateDB) -> Result<(u128, u128, Vec<StacksTransactionReceipt>), Error> {
        if self.last_mblock_seq == u16::max_value() {
            // no-op
            return Ok((0, 0, vec![]));
        }

        match self.load_child_microblocks(chainstate)? {
            Some(microblocks) => {
                self.append_microblocks(chainstate, burn_dbconn, microblocks)
            }
            None => {
                Ok((0, 0, vec![]))
            }
        }
    }
}

impl StacksChainState {
    /// Clear the current unconfirmed state
    fn drop_unconfirmed_state(&mut self, mut unconfirmed: UnconfirmedState) {
        debug!("Drop unconfirmed state off of {}", &unconfirmed.confirmed_chain_tip);
        let clarity_tx = StacksChainState::begin_unconfirmed(self.config(), &NULL_HEADER_DB, &mut unconfirmed.clarity_inst, &NULL_BURN_STATE_DB, &unconfirmed.confirmed_chain_tip);
        clarity_tx.rollback_unconfirmed();
    }

    /// Instantiate the unconfirmed state of a given chain tip.
    /// Pre-populate it with any microblock state we have.
    fn make_unconfirmed_state(&self, burn_dbconn: &dyn BurnStateDB, anchored_block_id: StacksBlockId, anchored_block_cost: ExecutionCost) -> Result<(UnconfirmedState, u128, u128, Vec<StacksTransactionReceipt>), Error> {
        let mut unconfirmed_state = UnconfirmedState::new(self, anchored_block_id, anchored_block_cost)?;
        let (fees, burns, receipts) = unconfirmed_state.refresh(self, burn_dbconn)?;
        Ok((unconfirmed_state, fees, burns, receipts))
    }

    /// Reload the unconfirmed view from a new chain tip.
    /// -- if the canonical chain tip hasn't changed, then just apply any new microblocks that have arrived.
    /// -- if the canonical chain tip has changed, then drop the current view, make a new view, and
    /// process that new view's unconfirmed microblocks.
    /// Call after storing all microblocks from the network.
    pub fn reload_unconfirmed_state(&mut self, burn_dbconn: &dyn BurnStateDB, canonical_tip: StacksBlockId) -> Result<(u128, u128, Vec<StacksTransactionReceipt>), Error> {
        debug!("Reload unconfirmed state off of {}", &canonical_tip);

        let unconfirmed_state = self.unconfirmed_state.take();

        if let Some(mut unconfirmed_state) = unconfirmed_state {
            if canonical_tip == unconfirmed_state.confirmed_chain_tip {
                // refresh with latest microblocks
                let res = unconfirmed_state.refresh(self, burn_dbconn);
                self.unconfirmed_state = Some(unconfirmed_state);
                return res;
            }
            else {
                self.unconfirmed_state = Some(unconfirmed_state);
            }
        }

        let block_cost = StacksChainState::get_stacks_block_anchored_cost(&self.headers_db, &canonical_tip)?
            .ok_or_else(|| Error::NoSuchBlockError)?;

        // tip changed, or we don't have unconfirmed state yet
        let (new_unconfirmed_state, fees, burns, receipts) = self.make_unconfirmed_state(burn_dbconn, canonical_tip, block_cost)?;
        if let Some(unconfirmed_state) = self.unconfirmed_state.take() {
            self.drop_unconfirmed_state(unconfirmed_state);
        }
        self.unconfirmed_state = Some(new_unconfirmed_state);
        Ok((fees, burns, receipts))
    }

    /// Refresh the current unconfirmed chain state
    pub fn refresh_unconfirmed_state(&mut self, burn_dbconn: &dyn BurnStateDB) -> Result<(u128, u128, Vec<StacksTransactionReceipt>), Error> {
        let mut unconfirmed_state = self.unconfirmed_state.take();
        let res = 
            if let Some(ref mut unconfirmed_state) = unconfirmed_state {
                debug!("Refresh unconfirmed state off of {}", &unconfirmed_state.confirmed_chain_tip);
                unconfirmed_state.refresh(self, burn_dbconn)
            }
            else {
                warn!("No unconfirmed state instantiated");
                Ok((0, 0, vec![]))
            };
        self.unconfirmed_state = unconfirmed_state;
        res
    }

    /// Refresh the current unconfirmed state in a read-only fashion -- just make sure it's
    /// pointing to the given stacks block ID.
    /// Don't apply any new microblocks.
    pub fn refresh_unconfirmed_state_readonly(&mut self, canonical_tip: StacksBlockId) -> Result<(), Error> {
        debug!("Refresh read-only unconfirmed state off of {}", &canonical_tip);
       
        let unconfirmed_state_opt = self.unconfirmed_state.take();
        if let Some(unconfirmed_state) = unconfirmed_state_opt {
            if unconfirmed_state.confirmed_chain_tip == canonical_tip {
                self.unconfirmed_state = Some(unconfirmed_state);
                Ok(())
            }
            else {
                let new_unconfirmed_state = UnconfirmedState::open_readonly(self, canonical_tip)?;
                self.unconfirmed_state = Some(new_unconfirmed_state);
                Ok(())
            }
        }
        else {
            let new_unconfirmed_state = UnconfirmedState::open_readonly(self, canonical_tip)?;
            self.unconfirmed_state = Some(new_unconfirmed_state);
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    
    use std::fs;

    use burnchains::PublicKey;
    
    use chainstate::stacks::index::*;
    use chainstate::stacks::index::node::*;
    use chainstate::stacks::index::marf::*;

    use chainstate::stacks::db::test::*;
    use chainstate::stacks::db::*;
    use chainstate::stacks::miner::*;
    use chainstate::stacks::*;

    use net::test::*;

    use chainstate::burn::db::*;
    use chainstate::burn::db::sortdb::*;

    use core::mempool::*;
    use chainstate::stacks::miner::test::make_coinbase;

    #[test]
    fn test_unconfirmed_refresh_one_microblock_stx_transfer() {
        let privk = StacksPrivateKey::new();
        let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

        let initial_balance = 1000000000;
        let mut peer_config = TestPeerConfig::new("test_unconfirmed_refresh_one_microblock_stx_transfer", 7000, 7001);
        peer_config.initial_balances = vec![
            (addr.to_account_principal(), initial_balance)
        ];

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let num_blocks = 10;
        let first_stacks_block_height = {
            let sn = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut last_block : Option<StacksBlock> = None;
        for tenure_id in 0..num_blocks {
            let microblock_privkey = StacksPrivateKey::new();
            let microblock_pubkeyhash = Hash160::from_data(&StacksPublicKey::from_private(&microblock_privkey).to_bytes());

            // send transactions to the mempool
            let tip = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn()).unwrap();

            assert_eq!(tip.block_height, first_stacks_block_height + (tenure_id as u64));
            if let Some(block) = last_block {
                assert_eq!(tip.winning_stacks_block_hash, block.block_hash());
            }

            let mut anchor_size = 0;
            let mut anchor_cost = ExecutionCost::zero();

            let (burn_ops, stacks_block, _) = peer.make_tenure(|ref mut miner, ref mut sortdb, ref mut chainstate, vrf_proof, ref parent_opt, _| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = sortdb.index_conn();
                        let snapshot = SortitionDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.sortition_id, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.consensus_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };

                let block_builder = StacksBlockBuilder::make_block_builder(&parent_tip, vrf_proof, tip.total_burn, microblock_pubkeyhash).unwrap();

                let coinbase_tx = make_coinbase(miner, tenure_id);
                let (anchored_block, anchored_block_size, anchored_block_cost) = StacksBlockBuilder::make_anchored_block_from_txs(block_builder, chainstate, &sortdb.index_conn(), vec![coinbase_tx]).unwrap();

                anchor_size = anchored_block_size;
                anchor_cost = anchored_block_cost;
                (anchored_block, vec![])
            });

            last_block = Some(stacks_block.clone());
            let (_, _, consensus_hash) = peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &vec![]);
            
            let recv_addr = StacksAddress::from_string("ST1H1B54MY50RMBRRKS7GV2ZWG79RZ1RQ1ETW4E01").unwrap();
           
            // build 1-block microblock stream
            let microblocks = {
                let sortdb = peer.sortdb.take().unwrap();
                let sort_iconn = sortdb.index_conn();
                let microblock = {
                    let mut microblock_builder = StacksMicroblockBuilder::new(stacks_block.block_hash(), consensus_hash.clone(), peer.chainstate(), &sort_iconn, anchor_cost.clone(), anchor_size).unwrap();

                    // make a single stx-transfer
                    let auth = TransactionAuth::Standard(TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(&privk)).unwrap());
                    let mut tx_stx_transfer = StacksTransaction::new(
                        TransactionVersion::Testnet, auth.clone(),
                        TransactionPayload::TokenTransfer(recv_addr.clone().into(), 1, TokenTransferMemo([0u8; 34])));

                    tx_stx_transfer.chain_id = 0x80000000;
                    tx_stx_transfer.post_condition_mode = TransactionPostConditionMode::Allow;
                    tx_stx_transfer.set_fee_rate(0);
                    tx_stx_transfer.set_origin_nonce(tenure_id as u64);

                    let mut signer = StacksTransactionSigner::new(&tx_stx_transfer);
                    signer.sign_origin(&privk).unwrap();

                    let signed_tx = signer.get_tx().unwrap();

                    let microblock = microblock_builder.mine_next_microblock_from_txs(vec![MemPoolTxInfo::from_tx(signed_tx, 0, consensus_hash.clone(), stacks_block.block_hash(), tenure_id as u64)], &microblock_privkey).unwrap();
                    microblock
                };

                peer.sortdb = Some(sortdb);
                vec![microblock]
            };

            // store microblock stream
            for mblock in microblocks.into_iter() {
                peer.chainstate().preprocess_streamed_microblock(&consensus_hash, &stacks_block.block_hash(), &mblock).unwrap();
            }

            // process microblock stream to generate unconfirmed state
            let sortdb = peer.sortdb.take().unwrap();
            let canonical_tip = StacksBlockHeader::make_index_block_hash(&consensus_hash, &stacks_block.block_hash());
            peer.chainstate().reload_unconfirmed_state(&sortdb.index_conn(), canonical_tip.clone()).unwrap();
    
            let recv_balance = peer.chainstate().with_read_only_unconfirmed_clarity_tx(&sortdb.index_conn(), |clarity_tx| {
                clarity_tx.with_clarity_db_readonly(|clarity_db| {
                    clarity_db.get_account_stx_balance(&recv_addr.into())
                })
            }).unwrap();
            peer.sortdb = Some(sortdb);
            
            // move 1 stx per round
            assert_eq!(recv_balance.amount_unlocked, (tenure_id + 1) as u128);
            let (canonical_burn, canonical_block) = SortitionDB::get_canonical_stacks_chain_tip_hash(peer.sortdb().conn()).unwrap();

            let sortdb = peer.sortdb.take().unwrap();
            let confirmed_recv_balance = peer.chainstate().with_read_only_clarity_tx(&sortdb.index_conn(), &canonical_tip, |clarity_tx| {
                clarity_tx.with_clarity_db_readonly(|clarity_db| {
                    clarity_db.get_account_stx_balance(&recv_addr.into())
                })
            });
            peer.sortdb = Some(sortdb);

            assert_eq!(confirmed_recv_balance.amount_unlocked, tenure_id as u128);
            eprintln!("\nrecv_balance: {}\nconfirmed_recv_balance: {}\nblock header {}: {:?}\ntip: {}/{}\n", recv_balance.amount_unlocked, confirmed_recv_balance.amount_unlocked, &stacks_block.block_hash(), &stacks_block.header, &canonical_burn, &canonical_block);
        }
    }
    
    #[test]
    fn test_unconfirmed_refresh_10_microblocks_10_stx_transfers() {
        let privk = StacksPrivateKey::new();
        let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

        let initial_balance = 1000000000;
        let mut peer_config = TestPeerConfig::new("test_unconfirmed_refresh_10_microblocks_10_stx_transfers", 7002, 7003);
        peer_config.initial_balances = vec![
            (addr.to_account_principal(), initial_balance)
        ];

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let num_blocks = 10;
        let first_stacks_block_height = {
            let tip = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn()).unwrap();
            tip.block_height
        };

        let mut last_block : Option<StacksBlock> = None;
        for tenure_id in 0..num_blocks {
            let microblock_privkey = StacksPrivateKey::new();
            let microblock_pubkeyhash = Hash160::from_data(&StacksPublicKey::from_private(&microblock_privkey).to_bytes());

            // send transactions to the mempool
            let tip = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn()).unwrap();

            assert_eq!(tip.block_height, first_stacks_block_height + (tenure_id as u64));
            if let Some(block) = last_block {
                assert_eq!(tip.winning_stacks_block_hash, block.block_hash());
            }

            let mut anchor_size = 0;
            let mut anchor_cost = ExecutionCost::zero();

            let (burn_ops, stacks_block, _) = peer.make_tenure(|ref mut miner, ref mut sortdb, ref mut chainstate, vrf_proof, ref parent_opt, _| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = sortdb.index_conn();
                        let snapshot = SortitionDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.sortition_id, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.consensus_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };

                let block_builder = StacksBlockBuilder::make_block_builder(&parent_tip, vrf_proof, tip.total_burn, microblock_pubkeyhash).unwrap();

                let coinbase_tx = make_coinbase(miner, tenure_id);
                let (anchored_block, anchored_block_size, anchored_block_cost) = StacksBlockBuilder::make_anchored_block_from_txs(block_builder, chainstate, &sortdb.index_conn(), vec![coinbase_tx]).unwrap();

                anchor_size = anchored_block_size;
                anchor_cost = anchored_block_cost;
                (anchored_block, vec![])
            });

            last_block = Some(stacks_block.clone());
            let (_, _, consensus_hash) = peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &vec![]);
            
            let recv_addr = StacksAddress::from_string("ST1H1B54MY50RMBRRKS7GV2ZWG79RZ1RQ1ETW4E01").unwrap();
           
            // build microblock stream iteratively, and test balances at each additional microblock
            let sortdb = peer.sortdb.take().unwrap();
            let microblocks = {
                let sort_iconn = sortdb.index_conn();
                let mut microblock_builder = StacksMicroblockBuilder::new(stacks_block.block_hash(), consensus_hash.clone(), peer.chainstate(), &sort_iconn, anchor_cost.clone(), anchor_size).unwrap();
                let mut microblocks = vec![];
                for i in 0..10 {
                    let mut signed_txs = vec![];
                    for j in 0..10 {
                        // make 10 stx-transfers in 10 microblocks (100 txs total)
                        let auth = TransactionAuth::Standard(TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(&privk)).unwrap());
                        let mut tx_stx_transfer = StacksTransaction::new(
                            TransactionVersion::Testnet, auth.clone(),
                            TransactionPayload::TokenTransfer(recv_addr.clone().into(), 1, TokenTransferMemo([0u8; 34])));

                        tx_stx_transfer.chain_id = 0x80000000;
                        tx_stx_transfer.post_condition_mode = TransactionPostConditionMode::Allow;
                        tx_stx_transfer.set_fee_rate(0);
                        tx_stx_transfer.set_origin_nonce((100*tenure_id + 10*i + j) as u64);

                        let mut signer = StacksTransactionSigner::new(&tx_stx_transfer);
                        signer.sign_origin(&privk).unwrap();

                        let signed_tx = signer.get_tx().unwrap();
                        signed_txs.push(signed_tx);
                    }

                    let signed_mempool_txs = signed_txs.into_iter()
                        .map(|tx| MemPoolTxInfo::from_tx(tx, 0, consensus_hash.clone(), stacks_block.block_hash(), tenure_id as u64))
                        .collect();

                    let microblock = microblock_builder.mine_next_microblock_from_txs(signed_mempool_txs, &microblock_privkey).unwrap();
                    microblocks.push(microblock);
                }
                microblocks
            };
            peer.sortdb = Some(sortdb);

            // store microblock stream
            for (i, mblock) in microblocks.into_iter().enumerate() {
                peer.chainstate().preprocess_streamed_microblock(&consensus_hash, &stacks_block.block_hash(), &mblock).unwrap();

                // process microblock stream to generate unconfirmed state
                let sortdb = peer.sortdb.take().unwrap();
                let canonical_tip = StacksBlockHeader::make_index_block_hash(&consensus_hash, &stacks_block.block_hash());
                peer.chainstate().reload_unconfirmed_state(&sortdb.index_conn(), canonical_tip.clone()).unwrap();
       
                let recv_balance = peer.chainstate().with_read_only_unconfirmed_clarity_tx(&sortdb.index_conn(), |clarity_tx| {
                    clarity_tx.with_clarity_db_readonly(|clarity_db| {
                        clarity_db.get_account_stx_balance(&recv_addr.into())
                    })
                }).unwrap();
                peer.sortdb = Some(sortdb);

                // move 100 ustx per round -- 10 per mblock
                assert_eq!(recv_balance.amount_unlocked, (100*tenure_id + 10*(i+1)) as u128);
                let (canonical_burn, canonical_block) = SortitionDB::get_canonical_stacks_chain_tip_hash(peer.sortdb().conn()).unwrap();

                let sortdb = peer.sortdb.take().unwrap();
                let confirmed_recv_balance = peer.chainstate().with_read_only_clarity_tx(&sortdb.index_conn(), &canonical_tip, |clarity_tx| {
                    clarity_tx.with_clarity_db_readonly(|clarity_db| {
                        clarity_db.get_account_stx_balance(&recv_addr.into())
                    })
                });
                peer.sortdb = Some(sortdb);

                assert_eq!(confirmed_recv_balance.amount_unlocked, 100*tenure_id as u128);
                eprintln!("\nrecv_balance: {}\nconfirmed_recv_balance: {}\nblock header {}: {:?}\ntip: {}/{}\n", recv_balance.amount_unlocked, confirmed_recv_balance.amount_unlocked, &stacks_block.block_hash(), &stacks_block.header, &canonical_burn, &canonical_block);
            }
        }
    }
}
