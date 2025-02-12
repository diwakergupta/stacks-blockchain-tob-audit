use super::{Config, BurnchainTip};
use super::node::{TESTNET_CHAIN_ID, ChainTip};

use std::time::{Instant, Duration};
use std::thread;

use stacks::burnchains::PublicKey;
use stacks::chainstate::stacks::db::{StacksChainState};
use stacks::chainstate::stacks::{StacksPrivateKey, StacksBlock, 
                                 StacksPublicKey, StacksTransaction, StacksMicroblock, StacksBlockBuilder};
use stacks::chainstate::burn::VRFSeed;
use stacks::core::mempool::MemPoolDB;
use stacks::util::vrf::VRFProof;
use stacks::util::hash::Hash160;

use stacks::vm::database::BurnStateDB;

pub struct TenureArtifacts {
    pub anchored_block: StacksBlock,
    pub microblocks: Vec<StacksMicroblock>,
    pub parent_block: BurnchainTip,
    pub burn_fee: u64
}

pub struct Tenure {
    coinbase_tx: StacksTransaction,
    config: Config,
    pub burnchain_tip: BurnchainTip,
    pub parent_block: ChainTip, 
    pub mem_pool: MemPoolDB,
    pub vrf_seed: VRFSeed,
    burn_fee_cap: u64,
    vrf_proof: VRFProof,
    microblock_pubkeyhash: Hash160,
    parent_block_total_burn: u64
}

impl <'a> Tenure {

    pub fn new(parent_block: ChainTip, 
               coinbase_tx: StacksTransaction,
               config: Config,
               mem_pool: MemPoolDB,
               microblock_secret_key: StacksPrivateKey,  
               burnchain_tip: BurnchainTip,
               vrf_proof: VRFProof,
               burn_fee_cap: u64) -> Tenure {

        let mut microblock_pubkey = StacksPublicKey::from_private(&microblock_secret_key);
        microblock_pubkey.set_compressed(true);
        let microblock_pubkeyhash = Hash160::from_data(&microblock_pubkey.to_bytes());

        let parent_block_total_burn = burnchain_tip.block_snapshot.total_burn;

        Self {
            coinbase_tx,
            config,
            burnchain_tip,
            mem_pool,
            parent_block,
            vrf_seed: VRFSeed::from_proof(&vrf_proof),
            vrf_proof,
            burn_fee_cap,
            microblock_pubkeyhash,
            parent_block_total_burn
        }
    }

    pub fn run(&mut self, burn_dbconn: &dyn BurnStateDB) -> Option<TenureArtifacts> {
        info!("Node starting new tenure with VRF {:?}", self.vrf_seed);

        let duration_left: u128 = self.config.burnchain.commit_anchor_block_within as u128;
        let mut elapsed = Instant::now().duration_since(self.burnchain_tip.received_at);
        while duration_left.saturating_sub(elapsed.as_millis()) > 0 {
            thread::sleep(Duration::from_millis(1000));
            elapsed = Instant::now().duration_since(self.burnchain_tip.received_at);
        } 


        let mut chain_state = StacksChainState::open_with_block_limit(
            false, 
            TESTNET_CHAIN_ID, 
            &self.config.get_chainstate_path(),
            self.config.block_limit.clone()).unwrap();

        let (anchored_block, _, _) = StacksBlockBuilder::build_anchored_block(
            &mut chain_state, burn_dbconn, &mut self.mem_pool, &self.parent_block.metadata,
            self.parent_block_total_burn, self.vrf_proof.clone(), self.microblock_pubkeyhash.clone(),
            &self.coinbase_tx, self.config.block_limit.clone()).unwrap();
    
        info!("Finish tenure: {}", anchored_block.block_hash());

        let artifact = TenureArtifacts {
            anchored_block,
            microblocks: vec![],
            parent_block: self.burnchain_tip.clone(),
            burn_fee: self.burn_fee_cap
        };
        Some(artifact)
    }
}
