#![no_main]

use libfuzzer_sys::fuzz_target;

use blockstack_lib::chainstate::burn::BlockHeaderHash;
use blockstack_lib::chainstate::stacks::index::storage::TrieFileStorage;

use blockstack_lib::vm::clarity::ClarityInstance;
use blockstack_lib::vm::database::{NULL_HEADER_DB, MarfedKV};
use blockstack_lib::vm::execute;
use blockstack_lib::vm::types::QualifiedContractIdentifier;

fuzz_target!(|data: &[u8]| {
    fuzz(data);
});

fn fuzz(data: &[u8]) {
    match std::str::from_utf8(data) {
        Ok(program) => {
            let contract_id = QualifiedContractIdentifier::transient();
            let mut clarity = ClarityInstance::new(MarfedKV::temporary());
            let mut block = clarity.begin_block(&TrieFileStorage::block_sentinel(),
                                                &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                &NULL_HEADER_DB);
            match block.analyze_smart_contract(&contract_id, &program) {
                Ok(_) => {
                    let _ = execute(program);
                },
                _ => {}
            }
        },
        Err(_) => {},
    };
}
