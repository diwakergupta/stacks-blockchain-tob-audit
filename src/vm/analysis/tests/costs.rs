use vm::execute as vm_execute;
use vm::errors::{Error, CheckErrors, RuntimeErrorType};
use vm::types::{Value, PrincipalData, ResponseData, QualifiedContractIdentifier, AssetIdentifier};
use vm::contexts::{OwnedEnvironment, GlobalContext, AssetMap, AssetMapEntry};
use vm::functions::NativeFunctions;
use vm::representations::SymbolicExpression;
use vm::contracts::Contract;
use util::hash::hex_bytes;
use vm::tests::{with_memory_environment, with_marfed_environment, symbols_from_values,
                execute };
use vm::clarity::ClarityInstance;

use vm::contexts::{Environment};
use vm::costs::{ExecutionCost};
use vm::database::{ClarityDatabase, MarfedKV, MemoryBackingStore,
                   NULL_BURN_STATE_DB, NULL_HEADER_DB};

use chainstate::stacks::index::storage::{TrieFileStorage};
use chainstate::burn::BlockHeaderHash;
use chainstate::stacks::index::MarfTrieId;
use chainstate::stacks::StacksBlockId;

use vm::tests::costs::get_simple_test;

pub fn test_tracked_costs(prog: &str) -> ExecutionCost {
    let marf = MarfedKV::temporary();
    let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());

    let p1 = execute("'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR");

    let p1_principal = match p1 {
        Value::Principal(PrincipalData::Standard(ref data)) => data.clone(),
        _ => panic!()
    };

    let contract_trait = "(define-trait trait-1 (
                            (foo-exec (int) (response int int))
                          ))";
    let contract_other = "(impl-trait .contract-trait.trait-1)
                          (define-map map-foo ((a int)) ((b int)))
                          (define-public (foo-exec (a int)) (ok 1))";

    let contract_self = format!("(define-map map-foo ((a int)) ((b int)))
                         (define-non-fungible-token nft-foo int)
                         (define-fungible-token ft-foo)
                         (define-data-var var-foo int 0)
                         (define-constant tuple-foo (tuple (a 1)))
                         (define-constant list-foo (list true))
                         (define-constant list-bar (list 1))
                         (use-trait trait-1 .contract-trait.trait-1)
                         (define-public (execute (contract <trait-1>)) (ok {}))", prog);

    let self_contract_id = QualifiedContractIdentifier::new(p1_principal.clone(), "self".into());
    let other_contract_id = QualifiedContractIdentifier::new(p1_principal.clone(), "contract-other".into());
    let trait_contract_id = QualifiedContractIdentifier::new(p1_principal.clone(), "contract-trait".into());

    {
        let mut conn = clarity_instance.begin_block(&StacksBlockId::sentinel(),
                                                    &StacksBlockId([0 as u8; 32]),
                                                    &NULL_HEADER_DB,
                                                    &NULL_BURN_STATE_DB);
        conn.as_transaction(|conn| {
            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&trait_contract_id, contract_trait).unwrap();
            conn.initialize_smart_contract(
                &trait_contract_id, &ct_ast, contract_trait, |_,_| false).unwrap();
            conn.save_analysis(&trait_contract_id, &ct_analysis).unwrap();
        });

        conn.commit_block();
    }

    {
        let mut conn = clarity_instance.begin_block(&StacksBlockId([0 as u8; 32]),
                                                    &StacksBlockId([1 as u8; 32]),
                                                    &NULL_HEADER_DB,
                                                    &NULL_BURN_STATE_DB);
        conn.as_transaction(|conn| {
            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&other_contract_id, contract_other).unwrap();
            conn.initialize_smart_contract(
                &other_contract_id, &ct_ast, contract_other, |_,_| false).unwrap();
            conn.save_analysis(&other_contract_id, &ct_analysis).unwrap();
        });

        conn.commit_block();
    }

    {
        let mut conn = clarity_instance.begin_block(&StacksBlockId([1 as u8; 32]),
                                                    &StacksBlockId([2 as u8; 32]),
                                                    &NULL_HEADER_DB,
                                                    &NULL_BURN_STATE_DB);

        conn.as_transaction(|conn| {
            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&self_contract_id, &contract_self).unwrap();
            conn.initialize_smart_contract(
                &self_contract_id, &ct_ast, &contract_self, |_,_| false).unwrap();
            conn.save_analysis(&self_contract_id, &ct_analysis).unwrap();
        });

        conn.commit_block().get_total()
    }
}

#[test]
fn test_all() {
    let baseline = test_tracked_costs("1");

    for f in NativeFunctions::ALL.iter() {
        let test = get_simple_test(f);
        let cost = test_tracked_costs(test);
        assert!(cost.exceeds(&baseline));
    }
}
