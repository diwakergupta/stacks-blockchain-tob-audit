use std::convert::{TryFrom, TryInto};
use std::cmp;

use vm::types::{Value, PrincipalData, QualifiedContractIdentifier};
use vm::representations::{SymbolicExpression, SymbolicExpressionType};
use vm::errors::Error;
use vm::errors::{CheckErrors, InterpreterError, RuntimeErrorType, InterpreterResult as Result};

use chainstate::stacks::boot::boot_code_id;
use chainstate::stacks::db::StacksChainState;
use vm::clarity::ClarityTransactionConnection;
use vm::database::ClarityDatabase;

fn parse_pox_stacking_result(result: &Value) -> std::result::Result<(PrincipalData, u128, u128), i128> {
    match result.clone().expect_result() {
        Ok(res) => {
            // should have gotten back (ok (tuple (stacker principal) (lock-amount uint) (unlock-burn-height uint)))
            let tuple_data = res.expect_tuple();
            let stacker = tuple_data
                .get("stacker")
                .expect(&format!("FATAL: no 'stacker'"))
                .to_owned()
                .expect_principal();

            let lock_amount = tuple_data
                .get("lock-amount")
                .expect(&format!("FATAL: no 'lock-amount'"))
                .to_owned()
                .expect_u128();

            let unlock_burn_height = tuple_data
                .get("unlock-burn-height")
                .expect(&format!("FATAL: no 'unlock-burn-height'"))
                .to_owned()
                .expect_u128();

            Ok((stacker, lock_amount, unlock_burn_height))
        }
        Err(e) => {
            Err(e.expect_i128())
        }
    }
}

/// Handle special cases when calling into the PoX API contract
fn handle_pox_api_contract_call(db: &mut ClarityDatabase, sender_opt: Option<&PrincipalData>, function_name: &str, value: &Value) -> Result<()> {
    if function_name == "stack-stx" {
        debug!("Handle special-case contract-call to {:?} {} (which returned {:?})", boot_code_id("pox"), function_name, value);

        // sender is required
        let sender = match sender_opt {
            None => {
                return Err(RuntimeErrorType::NoSenderInContext.into());
            }
            Some(sender) => (*sender).clone()
        };

        match parse_pox_stacking_result(value) {
            Ok((stacker, lock_amount, unlock_burn_height)) => {
                assert_eq!(stacker, sender, "BUG: tx-sender is not contract-call origin!");

                // if this fails, then there's a bug in the contract (since it already does
                // the necessary checks)
                match StacksChainState::pox_lock(db, &sender, lock_amount, unlock_burn_height as u64) {
                    Ok(_) => {},
                    Err(e) => {
                        panic!("FATAL: failed to lock {} from {} until {}: '{:?}'", lock_amount, sender, unlock_burn_height, &e);
                    }
                }

                return Ok(());
            },
            Err(_) => {
                // nothing to do -- the function failed
                return Ok(());
            }
        }
    }
    // nothing to do
    Ok(())
}

/// Handle special cases of contract-calls -- namely, those into PoX that should lock up STX
pub fn handle_contract_call_special_cases(db: &mut ClarityDatabase, sender: Option<&PrincipalData>, contract_id: &QualifiedContractIdentifier, function_name: &str, result: &Value) -> Result<()> {
    if *contract_id == boot_code_id("pox") {
        return handle_pox_api_contract_call(db, sender, function_name, result);
    }
    // TODO: insert more special cases here, as needed
    Ok(())
}

