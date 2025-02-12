use std::collections::{VecDeque, HashMap};
use std::convert::TryFrom;
use rusqlite::OptionalExtension;

use vm::contracts::Contract;
use vm::errors::{Error, InterpreterError, RuntimeErrorType, CheckErrors, InterpreterResult as Result, IncomparableError};
use vm::types::{Value, OptionalData, TypeSignature, TupleTypeSignature, PrincipalData, StandardPrincipalData, QualifiedContractIdentifier, NONE};

use chainstate::stacks::{
    StacksBlockId, StacksAddress
};
use chainstate::stacks::index::proofs::TrieMerkleProof;
use chainstate::stacks::db::{StacksHeaderInfo, MinerPaymentSchedule};
use chainstate::stacks::StacksBlockHeader;
use chainstate::burn::{VRFSeed, BlockHeaderHash, ConsensusHash};
use burnchains::BurnchainHeaderHash;

use util::hash::{Sha256Sum, Sha512Trunc256Sum};
use vm::database::{MarfedKV, ClarityBackingStore};
use vm::database::structures::{
    FungibleTokenMetadata, NonFungibleTokenMetadata, ContractMetadata,
    DataMapMetadata, DataVariableMetadata, ClaritySerializable, SimmedBlock,
    ClarityDeserializable, STXBalance
};
use vm::database::RollbackWrapper;
use util::db::{DBConn, FromRow};
use vm::costs::CostOverflowingMath;

use chainstate::burn::db::sortdb::{SortitionDBConn, SortitionHandleConn, SortitionDB, SortitionId};

use core::{
    FIRST_BURNCHAIN_BLOCK_HEIGHT,
    FIRST_BURNCHAIN_BLOCK_HASH,
    FIRST_BURNCHAIN_BLOCK_TIMESTAMP,
    POX_REWARD_CYCLE_LENGTH,
    FIRST_BURNCHAIN_CONSENSUS_HASH,
    FIRST_STACKS_BLOCK_HASH
};

pub const STORE_CONTRACT_SRC_INTERFACE: bool = true;

#[repr(u8)]
pub enum StoreType {
    DataMap = 0x00,
    Variable = 0x01,
    FungibleToken = 0x02,
    CirculatingSupply = 0x03,
    NonFungibleToken = 0x04,
    DataMapMeta = 0x05,
    VariableMeta = 0x06,
    FungibleTokenMeta = 0x07,
    NonFungibleTokenMeta = 0x08,
    Contract = 0x09,
    SimmedBlock = 0x10,
    SimmedBlockHeight = 0x11,
    Nonce = 0x12,
    STXBalance = 0x13,
    PoxSTXLockup = 0x14,
    PoxUnlockHeight = 0x15
}

pub struct ClarityDatabase<'a> {
    pub store: RollbackWrapper<'a>,
    headers_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
}

pub trait HeadersDB {
    fn get_stacks_block_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BlockHeaderHash>;
    fn get_burn_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BurnchainHeaderHash>;
    fn get_vrf_seed_for_block(&self, id_bhh: &StacksBlockId) -> Option<VRFSeed>;
    fn get_burn_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64>;
    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32>;
    fn get_miner_address(&self, id_bhh: &StacksBlockId) -> Option<StacksAddress>;
    fn get_total_liquid_ustx(&self, id_bhh: &StacksBlockId) -> u128;
}

pub trait BurnStateDB {
    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32>;
    fn get_burn_header_hash(&self, height: u32, sortition_id: &SortitionId) -> Option<BurnchainHeaderHash>;
}

fn get_stacks_header_info(conn: &DBConn, id_bhh: &StacksBlockId) -> Option<StacksHeaderInfo> {
    conn.query_row("SELECT * FROM block_headers WHERE index_block_hash = ?",
                   [id_bhh].iter(),
                   |x| StacksHeaderInfo::from_row(x).expect("Bad stacks header info in database"))
        .optional()
        .expect("Unexpected SQL failure querying block header table")
}

fn get_miner_info(conn: &DBConn, id_bhh: &StacksBlockId) -> Option<MinerPaymentSchedule> {
    conn.query_row("SELECT * FROM payments WHERE index_block_hash = ? AND miner = 1",
                   [id_bhh].iter(),
                   |x| MinerPaymentSchedule::from_row(x).expect("Bad payment info in database"))
        .optional()
        .expect("Unexpected SQL failure querying payment table")
}

impl HeadersDB for DBConn {
    fn get_stacks_block_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BlockHeaderHash> {
        get_stacks_header_info(self, id_bhh)
            .map(|x| x.anchored_header.block_hash())
    }
    
    fn get_burn_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BurnchainHeaderHash> {
        get_stacks_header_info(self, id_bhh)
            .map(|x| x.burn_header_hash)
    }

    fn get_burn_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        get_stacks_header_info(self, id_bhh)
            .map(|x| x.burn_header_timestamp)
    }

    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        get_stacks_header_info(self, id_bhh)
            .map(|x| x.burn_header_height)
    }

    fn get_vrf_seed_for_block(&self, id_bhh: &StacksBlockId) -> Option<VRFSeed> {
        get_stacks_header_info(self, id_bhh)
            .map(|x| VRFSeed::from_proof(&x.anchored_header.proof))
    }

    fn get_miner_address(&self, id_bhh: &StacksBlockId)  -> Option<StacksAddress> {
        get_miner_info(self, id_bhh)
            .map(|x| x.address)
    }

    fn get_total_liquid_ustx(&self, id_bhh: &StacksBlockId) -> u128 {
        get_stacks_header_info(self, id_bhh)
            .map(|x| x.total_liquid_ustx)
            .unwrap_or(0)
    }
}

impl HeadersDB for &dyn HeadersDB {
    fn get_stacks_block_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BlockHeaderHash> {
        (*self).get_stacks_block_header_hash_for_block(id_bhh)
    }
    fn get_burn_header_hash_for_block(&self, bhh: &StacksBlockId) -> Option<BurnchainHeaderHash> {
        (*self).get_burn_header_hash_for_block(bhh)
    }
    fn get_vrf_seed_for_block(&self, bhh: &StacksBlockId) -> Option<VRFSeed> {
        (*self).get_vrf_seed_for_block(bhh)
    }
    fn get_burn_block_time_for_block(&self, bhh: &StacksBlockId) -> Option<u64> {
        (*self).get_burn_block_time_for_block(bhh)
    }
    fn get_burn_block_height_for_block(&self, bhh: &StacksBlockId) -> Option<u32> {
        (*self).get_burn_block_height_for_block(bhh)
    }
    fn get_miner_address(&self, bhh: &StacksBlockId) -> Option<StacksAddress> {
        (*self).get_miner_address(bhh)
    }
    fn get_total_liquid_ustx(&self, bhh: &StacksBlockId) -> u128 {
        (*self).get_total_liquid_ustx(bhh)
    }
}

impl BurnStateDB for SortitionDBConn<'_> {
    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        match SortitionDB::get_block_snapshot(self.conn, sortition_id) {
            Ok(Some(x)) => Some(x.block_height as u32),
            _ => return None
        }
    }

    fn get_burn_header_hash(&self, height: u32, sortition_id: &SortitionId) -> Option<BurnchainHeaderHash> {
        let db_handle = SortitionHandleConn::open_reader(self, &sortition_id).ok()?;
        match db_handle.get_block_snapshot_by_height(height as u64) {
            Ok(Some(x)) => Some(x.burn_header_hash),
            _ => return None
        }
    }
} 

impl BurnStateDB for &dyn BurnStateDB {
    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        (*self).get_burn_block_height(sortition_id)
    }

    fn get_burn_header_hash(&self, height: u32, sortition_id: &SortitionId) -> Option<BurnchainHeaderHash> {
        (*self).get_burn_header_hash(height, sortition_id)
    }
}

pub struct NullHeadersDB {}
pub struct NullBurnStateDB {}

pub const NULL_HEADER_DB: NullHeadersDB = NullHeadersDB {};
pub const NULL_BURN_STATE_DB: NullBurnStateDB = NullBurnStateDB {};

impl HeadersDB for NullHeadersDB {
    fn get_burn_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BurnchainHeaderHash> {
        if *id_bhh == StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH) {
            Some(FIRST_BURNCHAIN_BLOCK_HASH)
        }
        else {
            None
        }
    }
    fn get_vrf_seed_for_block(&self, _bhh: &StacksBlockId) -> Option<VRFSeed> {
        None
    }
    fn get_stacks_block_header_hash_for_block(&self, id_bhh: &StacksBlockId) -> Option<BlockHeaderHash> {
        if *id_bhh == StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH) {
            Some(FIRST_STACKS_BLOCK_HASH)
        }
        else {
            None
        }
    }
    fn get_burn_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        if *id_bhh == StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH) {
            Some(FIRST_BURNCHAIN_BLOCK_TIMESTAMP)
        }
        else {
            None
        }
    }
    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        if *id_bhh == StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH) {
            Some(FIRST_BURNCHAIN_BLOCK_HEIGHT)
        }
        else {
            None
        }
    }
    fn get_miner_address(&self, _id_bhh: &StacksBlockId)  -> Option<StacksAddress> {
        None
    }
    fn get_total_liquid_ustx(&self, _id_bhh: &StacksBlockId) -> u128 {
        0
    }
}

impl BurnStateDB for NullBurnStateDB {
    fn get_burn_block_height(&self, _sortition_id: &SortitionId) -> Option<u32> {
        None
    }
    
    fn get_burn_header_hash(&self, _height: u32, _sortition_id: &SortitionId) -> Option<BurnchainHeaderHash> {
        None
    }
}

impl <'a> ClarityDatabase <'a> {
    pub fn new(store: &'a mut dyn ClarityBackingStore, headers_db: &'a dyn HeadersDB, burn_state_db: &'a dyn BurnStateDB) -> ClarityDatabase<'a> {
        ClarityDatabase {
            store: RollbackWrapper::new(store),
            headers_db,
            burn_state_db
        }
    }

    pub fn new_with_rollback_wrapper(store: RollbackWrapper<'a>, headers_db: &'a dyn HeadersDB, burn_state_db: &'a dyn BurnStateDB) -> ClarityDatabase<'a> {
        ClarityDatabase { store, headers_db, burn_state_db }
    }

    pub fn initialize(&mut self) {
    }

    pub fn begin(&mut self) {
        self.store.nest();
    }

    pub fn commit(&mut self) {
        self.store.commit();
    }

    pub fn roll_back(&mut self) {
        self.store.rollback();
    }

    pub fn set_block_hash(&mut self, bhh: StacksBlockId) -> Result<StacksBlockId> {
        self.store.set_block_hash(bhh)
    }

    pub fn put <T: ClaritySerializable> (&mut self, key: &str, value: &T) {
        self.store.put(&key, &value.serialize());
    }

    pub fn get <T> (&mut self, key: &str) -> Option<T> where T: ClarityDeserializable<T> {
        self.store.get::<T>(key)
    }

    pub fn get_value (&mut self, key: &str, expected: &TypeSignature) -> Option<Value> {
        self.store.get_value(key, expected)
    }

    pub fn get_with_proof <T> (&mut self, key: &str) -> Option<(T, TrieMerkleProof<StacksBlockId>)> where T: ClarityDeserializable<T> {
        self.store.get_with_proof(key)
    }

    pub fn make_key_for_trip(contract_identifier: &QualifiedContractIdentifier, data: StoreType, var_name: &str) -> String {
        format!("vm::{}::{}::{}", contract_identifier, data as u8, var_name)
    }

    pub fn make_metadata_key(data: StoreType, var_name: &str) -> String {
        format!("vm-metadata::{}::{}", data as u8, var_name)
    }

    pub fn make_key_for_quad(contract_identifier: &QualifiedContractIdentifier, data: StoreType, var_name: &str, key_value: String) -> String {
        format!("vm::{}::{}::{}::{}", contract_identifier, data as u8, var_name, key_value)
    }

    pub fn insert_contract_hash(&mut self, contract_identifier: &QualifiedContractIdentifier, contract_content: &str) -> Result<()> {
        let hash = Sha512Trunc256Sum::from_data(contract_content.as_bytes());
        self.store.prepare_for_contract_metadata(contract_identifier, hash);
        // insert contract-size
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-size");
        self.insert_metadata(contract_identifier, &key,
                             &(contract_content.len() as u64));

        // insert contract-src
        if STORE_CONTRACT_SRC_INTERFACE {
            let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-src");
            self.insert_metadata(contract_identifier, &key, &contract_content.to_string());
        }
        Ok(())
    }

    pub fn get_contract_src(&mut self, contract_identifier: &QualifiedContractIdentifier) -> Option<String> {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-src");
        self.fetch_metadata(contract_identifier, &key).ok().flatten()
    }

    fn insert_metadata <T: ClaritySerializable> (&mut self, contract_identifier: &QualifiedContractIdentifier, key: &str, data: &T) {
        if self.store.has_metadata_entry(contract_identifier, key) {
            panic!("Metadata entry '{}' already exists for contract: {}", key, contract_identifier);
        } else {
            self.store.insert_metadata(contract_identifier, key, &data.serialize());
        }
    }

    fn fetch_metadata <T> (&mut self, contract_identifier: &QualifiedContractIdentifier, key: &str) -> Result<Option<T>>
    where T: ClarityDeserializable<T> {
        self.store.get_metadata(contract_identifier, key)
            .map(|x_opt| x_opt.map(|x| T::deserialize(&x)))
    }

    pub fn get_contract_size(&mut self, contract_identifier: &QualifiedContractIdentifier) -> Result<u64> {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-size");
        let contract_size: u64 = self.fetch_metadata(contract_identifier, &key)?
            .expect("Failed to read non-consensus contract metadata, even though contract exists in MARF.");
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-data-size");
        let data_size: u64 = self.fetch_metadata(contract_identifier, &key)?
            .expect("Failed to read non-consensus contract metadata, even though contract exists in MARF.");

        // u64 overflow is _checked_ on insert into contract-data-size
        Ok(data_size + contract_size)
    }

    /// used for adding the memory usage of `define-constant` variables.
    pub fn set_contract_data_size(&mut self, contract_identifier: &QualifiedContractIdentifier, data_size: u64) -> Result<()> {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-size");
        let contract_size: u64 = self.fetch_metadata(contract_identifier, &key)?
            .expect("Failed to read non-consensus contract metadata, even though contract exists in MARF.");
        contract_size.cost_overflow_add(data_size)?;

        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract-data-size");
        self.insert_metadata(contract_identifier, &key, &data_size);
        Ok(())
    }

    pub fn insert_contract(&mut self, contract_identifier: &QualifiedContractIdentifier, contract: Contract) {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract");
        self.insert_metadata(contract_identifier, &key, &contract);
    }

    pub fn has_contract(&mut self, contract_identifier: &QualifiedContractIdentifier) -> bool {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract");
        self.store.has_metadata_entry(contract_identifier, &key)
    }

    pub fn get_contract(&mut self, contract_identifier: &QualifiedContractIdentifier) -> Result<Contract> {
        let key = ClarityDatabase::make_metadata_key(StoreType::Contract, "contract");
        let data = self.fetch_metadata(contract_identifier, &key)?
            .expect("Failed to read non-consensus contract metadata, even though contract exists in MARF.");
        Ok(data)
    }

    pub fn destroy(self) -> RollbackWrapper<'a> {
        self.store
    }
    
    pub fn is_in_regtest(&self) -> bool {
        cfg!(test)
    }
}

// Get block information

impl <'a> ClarityDatabase <'a> {
    pub fn get_index_block_header_hash(&mut self, block_height: u32) -> StacksBlockId {
        self.store.get_block_header_hash(block_height)
        // the caller is responsible for ensuring that the block_height given
        //  is < current_block_height, so this should _always_ return a value.
            .expect("Block header hash must return for provided block height")
    }

    pub fn get_current_block_height(&mut self) -> u32 {
        self.store.get_current_block_height()
    }

    /// Get the last-known burnchain block height.
    /// Note that this is _not_ the burnchain height in which this block was mined!
    /// This is the burnchain block height of its parent.
    pub fn get_current_burnchain_block_height(&mut self) -> u32 {
        let cur_stacks_height = self.store.get_current_block_height();
        let last_mined_bhh = 
            if cur_stacks_height == 0 {
                StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH)
            }
            else {
                self.get_index_block_header_hash(cur_stacks_height.checked_sub(1).expect("BUG: cannot eval burn-block-height in boot code"))
            };

        self.get_burnchain_block_height(&last_mined_bhh)
            .expect(&format!("Block header hash '{}' must return for provided stacks block height {}", &last_mined_bhh, cur_stacks_height))
    }

    pub fn get_block_header_hash(&mut self, block_height: u32) -> BlockHeaderHash {
        let id_bhh = self.get_index_block_header_hash(block_height);
        self.headers_db.get_stacks_block_header_hash_for_block(&id_bhh)
            .expect("Failed to get block data.")
    }

    pub fn get_block_time(&mut self, block_height: u32) -> u64 {
        let id_bhh = self.get_index_block_header_hash(block_height);
        self.headers_db.get_burn_block_time_for_block(&id_bhh)
            .expect("Failed to get block data.")
    }

    pub fn get_burnchain_block_header_hash(&mut self, block_height: u32) -> BurnchainHeaderHash {
        let id_bhh = self.get_index_block_header_hash(block_height);
        self.headers_db.get_burn_header_hash_for_block(&id_bhh)
            .expect("Failed to get block data.")
    }
    
    pub fn get_burnchain_block_height(&mut self, id_bhh: &StacksBlockId) -> Option<u32> {
        self.headers_db.get_burn_block_height_for_block(id_bhh)
    }

    pub fn get_block_vrf_seed(&mut self, block_height: u32) -> VRFSeed {
        let id_bhh = self.get_index_block_header_hash(block_height);
        self.headers_db.get_vrf_seed_for_block(&id_bhh)
            .expect("Failed to get block data.")
    }

    pub fn get_miner_address(&mut self, block_height: u32) -> StandardPrincipalData {
        let id_bhh = self.get_index_block_header_hash(block_height);
        self.headers_db.get_miner_address(&id_bhh)
            .expect("Failed to get block data.")
            .into()
    }

    pub fn get_total_liquid_ustx(&mut self) -> u128 {
        let cur_height = self.get_current_block_height();
        let cur_id_bhh = self.get_index_block_header_hash(cur_height);
        self.headers_db.get_total_liquid_ustx(&cur_id_bhh)
    }
}

// this is used so that things like load_map, load_var, load_nft, etc.
//   will throw NoSuchFoo errors instead of NoSuchContract errors.
fn map_no_contract_as_none <T> (res: Result<Option<T>>) -> Result<Option<T>> {
    res.or_else(|e| match e {
        Error::Unchecked(CheckErrors::NoSuchContract(_)) => Ok(None),
        x => Err(x)
    })
}

// Variable Functions...
impl <'a> ClarityDatabase <'a> {
    pub fn create_variable(&mut self, contract_identifier: &QualifiedContractIdentifier, variable_name: &str, value_type: TypeSignature) {
        let variable_data = DataVariableMetadata { value_type };
        let key = ClarityDatabase::make_metadata_key(StoreType::VariableMeta, variable_name);

        self.insert_metadata(contract_identifier, &key, &variable_data)
    }

    pub fn load_variable(&mut self, contract_identifier: &QualifiedContractIdentifier, variable_name: &str) -> Result<DataVariableMetadata> {
        let key = ClarityDatabase::make_metadata_key(StoreType::VariableMeta, variable_name);

        map_no_contract_as_none(
            self.fetch_metadata(contract_identifier, &key))?
            .ok_or(CheckErrors::NoSuchDataVariable(variable_name.to_string()).into())
    }

    pub fn set_variable(&mut self, contract_identifier: &QualifiedContractIdentifier, variable_name: &str, value: Value) -> Result<Value> {
        let variable_descriptor = self.load_variable(contract_identifier, variable_name)?;
        if !variable_descriptor.value_type.admits(&value) {
            return Err(CheckErrors::TypeValueError(variable_descriptor.value_type, value).into())
        }

        let key = ClarityDatabase::make_key_for_trip(contract_identifier, StoreType::Variable, variable_name);

        self.put(&key, &value);

        return Ok(Value::Bool(true))
    }

    pub fn lookup_variable(&mut self, contract_identifier: &QualifiedContractIdentifier, variable_name: &str) -> Result<Value>  {
        let variable_descriptor = self.load_variable(contract_identifier, variable_name)?;

        let key = ClarityDatabase::make_key_for_trip(contract_identifier, StoreType::Variable, variable_name);

        let result = self.get_value(&key, &variable_descriptor.value_type);

        match result {
            None => Ok(Value::none()),
            Some(data) => Ok(data)
        }
    }
}

// Data Map Functions
impl <'a> ClarityDatabase <'a> {
    pub fn create_map(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key_type: TupleTypeSignature, value_type: TupleTypeSignature) {
        let key_type = TypeSignature::from(key_type);
        let value_type = TypeSignature::from(value_type);

        let data = DataMapMetadata { key_type, value_type };

        let key = ClarityDatabase::make_metadata_key(StoreType::DataMapMeta, map_name);
        self.insert_metadata(contract_identifier, &key, &data)
    }

    pub fn load_map(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str) -> Result<DataMapMetadata> {
        let key = ClarityDatabase::make_metadata_key(StoreType::DataMapMeta, map_name);

        map_no_contract_as_none(
            self.fetch_metadata(contract_identifier, &key))?
            .ok_or(CheckErrors::NoSuchMap(map_name.to_string()).into())
    }

    pub fn make_key_for_data_map_entry(contract_identifier: &QualifiedContractIdentifier, map_name: &str, key_value: &Value) -> String {
        ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::DataMap, map_name, key_value.serialize())
    }

    pub fn fetch_entry(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key_value: &Value) -> Result<Value> {
        let map_descriptor = self.load_map(contract_identifier, map_name)?;
        if !map_descriptor.key_type.admits(key_value) {
            return Err(CheckErrors::TypeValueError(map_descriptor.key_type, (*key_value).clone()).into())
        }

        let key = ClarityDatabase::make_key_for_data_map_entry(contract_identifier, map_name, key_value);

        let stored_type = TypeSignature::new_option(map_descriptor.value_type)?;
        let result = self.get_value(&key, &stored_type);

        match result {
            None => Ok(Value::none()),
            Some(data) => Ok(data)
        }
    }

    pub fn set_entry(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key: Value, value: Value) -> Result<Value> {
        self.inner_set_entry(contract_identifier, map_name, key, value, false)
    }

    pub fn insert_entry(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key: Value, value: Value) -> Result<Value> {
        self.inner_set_entry(contract_identifier, map_name, key, value, true)
    }

    fn data_map_entry_exists(&mut self, key: &str, expected_value: &TypeSignature) -> Result<bool> {
        match self.get_value(key, expected_value) {
            None => Ok(false),
            Some(value) =>
                Ok(value != Value::none())
        }
    }
    
    fn inner_set_entry(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key_value: Value, value: Value, return_if_exists: bool) -> Result<Value> {
        let map_descriptor = self.load_map(contract_identifier, map_name)?;
        if !map_descriptor.key_type.admits(&key_value) {
            return Err(CheckErrors::TypeValueError(map_descriptor.key_type, key_value).into())
        }
        if !map_descriptor.value_type.admits(&value) {
            return Err(CheckErrors::TypeValueError(map_descriptor.value_type, value).into())
        }

        let key = ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::DataMap, map_name, key_value.serialize());
        let stored_type = TypeSignature::new_option(map_descriptor.value_type)?;

        if return_if_exists && self.data_map_entry_exists(&key, &stored_type)? {
            return Ok(Value::Bool(false))
        }

        let placed_value = Value::some(value)?;
        self.put(&key, &placed_value);

        return Ok(Value::Bool(true))
    }

    pub fn delete_entry(&mut self, contract_identifier: &QualifiedContractIdentifier, map_name: &str, key_value: &Value) -> Result<Value> {
        let map_descriptor = self.load_map(contract_identifier, map_name)?;
        if !map_descriptor.key_type.admits(key_value) {
            return Err(CheckErrors::TypeValueError(map_descriptor.key_type, (*key_value).clone()).into())
        }

        let key = ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::DataMap, map_name, key_value.serialize());
        let stored_type = TypeSignature::new_option(map_descriptor.value_type)?;
        if !self.data_map_entry_exists(&key, &stored_type)? {
            return Ok(Value::Bool(false))
        }

        self.put(&key, &(Value::none()));

        return Ok(Value::Bool(true))
    }
}

// Asset Functions

impl <'a> ClarityDatabase <'a> {
    pub fn create_fungible_token(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str, total_supply: &Option<u128>) {
        let data = FungibleTokenMetadata { total_supply: total_supply.clone() };

        let key = ClarityDatabase::make_metadata_key(StoreType::FungibleTokenMeta, token_name);
        self.insert_metadata(contract_identifier, &key, &data);

        // total supply _is_ included in the consensus hash
        if total_supply.is_some() {
            let supply_key = ClarityDatabase::make_key_for_trip(contract_identifier, StoreType::CirculatingSupply, token_name);
            self.put(&supply_key, &(0 as u128));
        }
    }

    fn load_ft(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str) -> Result<FungibleTokenMetadata> {
        let key = ClarityDatabase::make_metadata_key(StoreType::FungibleTokenMeta, token_name);

        map_no_contract_as_none(
            self.fetch_metadata(contract_identifier, &key))?
            .ok_or(CheckErrors::NoSuchFT(token_name.to_string()).into())
    }

    pub fn create_non_fungible_token(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str, key_type: &TypeSignature) {
        let data = NonFungibleTokenMetadata { key_type: key_type.clone() };
        let key = ClarityDatabase::make_metadata_key(StoreType::NonFungibleTokenMeta, token_name);
        self.insert_metadata(contract_identifier, &key, &data);
    }

    fn load_nft(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str) -> Result<NonFungibleTokenMetadata> {
        let key = ClarityDatabase::make_metadata_key(StoreType::NonFungibleTokenMeta, token_name);

        map_no_contract_as_none(
            self.fetch_metadata(contract_identifier, &key))?
            .ok_or(CheckErrors::NoSuchNFT(token_name.to_string()).into())
    }

    pub fn checked_increase_token_supply(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str, amount: u128) -> Result<()> {
        let descriptor = self.load_ft(contract_identifier, token_name)?;

        if let Some(total_supply) = descriptor.total_supply {
            let key = ClarityDatabase::make_key_for_trip(contract_identifier, StoreType::CirculatingSupply, token_name);
            let current_supply: u128 = self.get(&key)
                .expect("ERROR: Clarity VM failed to track token supply.");
 
            let new_supply = current_supply.checked_add(amount)
                .ok_or(RuntimeErrorType::ArithmeticOverflow)?;

            if new_supply > total_supply {
                Err(RuntimeErrorType::SupplyOverflow(new_supply, total_supply).into())
            } else {
                self.put(&key, &new_supply);
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    pub fn get_ft_balance(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str, principal: &PrincipalData) -> Result<u128> {
        self.load_ft(contract_identifier, token_name)?;

        let key =  ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::FungibleToken, token_name, principal.serialize());

        let result = self.get(&key);
        match result {
            None => Ok(0),
            Some(balance) => Ok(balance)
        }
    }

    pub fn set_ft_balance(&mut self, contract_identifier: &QualifiedContractIdentifier, token_name: &str, principal: &PrincipalData, balance: u128) -> Result<()> {
        let key =  ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::FungibleToken, token_name, principal.serialize());
        self.put(&key, &balance);

        Ok(())
    }

    pub fn get_nft_owner(&mut self, contract_identifier: &QualifiedContractIdentifier, asset_name: &str, asset: &Value) -> Result<PrincipalData> {
        let descriptor = self.load_nft(contract_identifier, asset_name)?;
        if !descriptor.key_type.admits(asset) {
            return Err(CheckErrors::TypeValueError(descriptor.key_type, (*asset).clone()).into())
        }

        let key = ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::NonFungibleToken, asset_name, asset.serialize());

        let result = self.get(&key);
        result.ok_or(RuntimeErrorType::NoSuchToken.into())
    }

    pub fn get_nft_key_type(&mut self, contract_identifier: &QualifiedContractIdentifier, asset_name: &str) -> Result<TypeSignature> {
        let descriptor = self.load_nft(contract_identifier, asset_name)?;
        Ok(descriptor.key_type)
    }

    pub fn set_nft_owner(&mut self, contract_identifier: &QualifiedContractIdentifier, asset_name: &str, asset: &Value, principal: &PrincipalData) -> Result<()> {
        let descriptor = self.load_nft(contract_identifier, asset_name)?;
        if !descriptor.key_type.admits(asset) {
            return Err(CheckErrors::TypeValueError(descriptor.key_type, (*asset).clone()).into())
        }

        let key = ClarityDatabase::make_key_for_quad(contract_identifier, StoreType::NonFungibleToken, asset_name, asset.serialize());

        self.put(&key, principal);

        Ok(())
    }
}

// load/store STX token state and account nonces
impl<'a> ClarityDatabase<'a> {
    fn make_key_for_account(principal: &PrincipalData, data: StoreType) -> String {
        format!("vm-account::{}::{}", principal, data as u8)
    }

    pub fn make_key_for_account_balance(principal: &PrincipalData) -> String {
        ClarityDatabase::make_key_for_account(principal, StoreType::STXBalance)
    }

    pub fn make_key_for_account_nonce(principal: &PrincipalData) -> String {
        ClarityDatabase::make_key_for_account(principal, StoreType::Nonce)
    }

    pub fn make_key_for_account_stx_locked(principal: &PrincipalData) -> String {
        ClarityDatabase::make_key_for_account(principal, StoreType::PoxSTXLockup)
    }

    pub fn make_key_for_account_unlock_height(principal: &PrincipalData) -> String {
        ClarityDatabase::make_key_for_account(principal, StoreType::PoxUnlockHeight)
    }

    pub fn get_account_stx_balance(&mut self, principal: &PrincipalData) -> STXBalance {
        let key = ClarityDatabase::make_key_for_account_balance(principal);
        let result = self.get(&key);
        match result {
            None => STXBalance::zero(),
            Some(balance) => balance
        }
    }

    pub fn set_account_stx_balance(&mut self, principal: &PrincipalData, balance: &STXBalance) {
        let key = ClarityDatabase::make_key_for_account_balance(principal);
        self.put(&key, balance);
    }

    pub fn get_account_nonce(&mut self, principal: &PrincipalData) -> u64 {
        let key = ClarityDatabase::make_key_for_account_nonce(principal);
        let result = self.get(&key);
        match result {
            None => 0,
            Some(nonce) => nonce
        }
    }

    pub fn set_account_nonce(&mut self, principal: &PrincipalData, nonce: u64) {
        let key = ClarityDatabase::make_key_for_account_nonce(principal);
        self.put(&key, &nonce);
    }
}

// access burnchain state
impl <'a> ClarityDatabase<'a> {
    pub fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        self.burn_state_db.get_burn_block_height(sortition_id)
    }

    pub fn get_burn_header_hash(&self, height: u32, sortition_id: &SortitionId) -> Option<BurnchainHeaderHash> {
        self.burn_state_db.get_burn_header_hash(height, sortition_id)
    }
}
