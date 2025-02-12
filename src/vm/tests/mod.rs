use vm::execute as vm_execute;
use vm::errors::{Error};
use vm::types::{Value, PrincipalData, ResponseData};
use vm::contexts::{OwnedEnvironment,GlobalContext, Environment};
use vm::representations::SymbolicExpression;
use vm::contracts::Contract;
use util::hash::hex_bytes;
use vm::database::{ClarityDatabase, MarfedKV, MemoryBackingStore,
                   NULL_HEADER_DB, NULL_BURN_STATE_DB};

use chainstate::stacks::index::storage::{TrieFileStorage};
use chainstate::stacks::index::MarfTrieId;
use chainstate::stacks::StacksBlockId;
use chainstate::stacks::StacksBlockHeader;

use core::{
    FIRST_BURNCHAIN_CONSENSUS_HASH,
    FIRST_STACKS_BLOCK_HASH
};

mod forking;
mod assets;
mod events;
mod sequences;
mod defines;
mod simple_apply_eval;
mod datamaps;
mod contracts;
pub mod costs;
mod traits;
mod large_contract;

pub fn with_memory_environment<F>(f: F, top_level: bool)
where F: FnOnce(&mut OwnedEnvironment) -> ()
{
    let mut marf_kv = MemoryBackingStore::new();

    let mut owned_env = OwnedEnvironment::new(marf_kv.as_clarity_db());
    // start an initial transaction.
    if !top_level {
        owned_env.begin();
    }

    f(&mut owned_env)
}

pub fn with_marfed_environment<F>(f: F, top_level: bool)
where F: FnOnce(&mut OwnedEnvironment) -> ()
{
    let mut marf_kv = MarfedKV::temporary();
    marf_kv.begin(&StacksBlockId::sentinel(),
                  &StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH));

    {
        marf_kv.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB).initialize();
    }

    marf_kv.test_commit();
    marf_kv.begin(&StacksBlockHeader::make_index_block_hash(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH),
                  &StacksBlockId([1 as u8; 32]));

    {
        let mut owned_env = OwnedEnvironment::new(marf_kv.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB));
        // start an initial transaction.
        if !top_level {
            owned_env.begin();
        }

        f(&mut owned_env)
    }
}

pub fn execute(s: &str) -> Value {
    vm_execute(s).unwrap().unwrap()
}

pub fn symbols_from_values(mut vec: Vec<Value>) -> Vec<SymbolicExpression> {
    vec.drain(..).map(|value| SymbolicExpression::atom_value(value)).collect()
}


fn is_committed(v: &Value) -> bool {
    eprintln!("is_committed?: {}", v);

    match v {
        Value::Response(ref data) => data.committed,
        _ => false
    }
}

fn is_err_code(v: &Value, e: u128) -> bool {
    eprintln!("is_err_code?: {}", v);
    match v {
        Value::Response(ref data) => {
            !data.committed &&
                *data.data == Value::UInt(e)
        },
        _ => false
    }
}
