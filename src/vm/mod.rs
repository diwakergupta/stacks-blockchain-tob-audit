extern crate regex;

pub mod errors;
pub mod diagnostic;

#[macro_use]
pub mod costs;

pub mod types;

pub mod contracts;

pub mod representations;
pub mod ast;
pub mod contexts;
pub mod database;
pub mod clarity;

mod functions;
mod variables;
mod callables;

pub mod docs;
pub mod analysis;

#[cfg(test)]
pub mod tests;

pub use vm::types::Value;
use vm::callables::CallableType;
use vm::contexts::{ContractContext, LocalContext, Environment, CallStack};
use vm::contexts::{GlobalContext};
use vm::functions::define::DefineResult;
use vm::errors::{Error, InterpreterError, RuntimeErrorType, CheckErrors, InterpreterResult as Result};
use vm::database::MemoryBackingStore;
use vm::types::{QualifiedContractIdentifier, TraitIdentifier, PrincipalData, TypeSignature};
use vm::costs::{cost_functions, CostOverflowingMath, LimitedCostTracker, MemoryConsumer, CostTracker};

pub use vm::representations::{SymbolicExpression, SymbolicExpressionType, ClarityName, ContractName};

pub use vm::functions::{get_stx_balance_snapshot, stx_transfer_consolidated};
pub use vm::contexts::MAX_CONTEXT_DEPTH;
use std::convert::TryInto;

const MAX_CALL_STACK_DEPTH: usize = 64;

fn lookup_variable(name: &str, context: &LocalContext, env: &mut Environment) -> Result<Value> {
    if name.starts_with(char::is_numeric) || name.starts_with('\'') {
        Err(InterpreterError::BadSymbolicRepresentation(format!("Unexpected variable name: {}", name)).into())
    } else {
        if let Some(value) = variables::lookup_reserved_variable(name, context, env)? {
            Ok(value)
        } else {
            runtime_cost!(cost_functions::LOOKUP_VARIABLE_DEPTH, env, context.depth())?;
            if let Some(value) = context.lookup_variable(name).or_else(
                || env.contract_context.lookup_variable(name)) {
                runtime_cost!(cost_functions::LOOKUP_VARIABLE_SIZE, env, value.size())?;
                Ok(value.clone())
            }  else if let Some(value) = context.lookup_callable_contract(name) {
                let contract_identifier = &value.0;
                Ok(Value::Principal(PrincipalData::Contract(contract_identifier.clone())))
            } else {
                Err(CheckErrors::UndefinedVariable(name.to_string()).into())
            }
        }
    }
}

pub fn lookup_function(name: &str, env: &mut Environment)-> Result<CallableType> {
    runtime_cost!(cost_functions::LOOKUP_FUNCTION, env, 0)?;

    if let Some(result) = functions::lookup_reserved_functions(name) {
        Ok(result)
    } else {
        let user_function = env.contract_context.lookup_function(name).ok_or(
            CheckErrors::UndefinedFunction(name.to_string()))?;
        Ok(CallableType::UserFunction(user_function))
    }
}

fn add_stack_trace(result: &mut Result<Value>, env: &Environment) {
    if let Err(Error::Runtime(_, ref mut stack_trace)) = result {
        if stack_trace.is_none() {
            stack_trace.replace(env.call_stack.make_stack_trace());
        }
    }
}

pub fn apply(function: &CallableType, args: &[SymbolicExpression],
             env: &mut Environment, context: &LocalContext) -> Result<Value> {
    let identifier = function.get_identifier();
    // Aaron: in non-debug executions, we shouldn't track a full call-stack.
    //        only enough to do recursion detection.

    // do recursion check on user functions.
    let track_recursion = match function {
        CallableType::UserFunction(_) => true,
        _ => false
    };

    if track_recursion && env.call_stack.contains(&identifier) {
        return Err(CheckErrors::CircularReference(vec![identifier.to_string()]).into())
    }

    if env.call_stack.depth() >= MAX_CALL_STACK_DEPTH {
        return Err(RuntimeErrorType::MaxStackDepthReached.into())
    }

    if let CallableType::SpecialFunction(_, function) = function {
        env.call_stack.insert(&identifier, track_recursion);
        let mut resp = function(args, env, context);
        add_stack_trace(&mut resp, env);
        env.call_stack.remove(&identifier, track_recursion)?;
        resp
    } else {
        env.call_stack.insert(&identifier, track_recursion);

        let mut used_memory = 0;
        let mut evaluated_args = vec![];
        for arg_x in args.iter() {
            let arg_value = match eval(arg_x, env, context) {
                Ok(x) => x,
                Err(e) => {
                    env.drop_memory(used_memory);
                    env.call_stack.remove(&identifier, track_recursion)?;
                    return Err(e)
                }
            };
            let arg_use = arg_value.get_memory_use();
            match env.add_memory(arg_use) {
                Ok(_x) => {},
                Err(e) => {
                    env.drop_memory(used_memory);
                    env.call_stack.remove(&identifier, track_recursion)?;
                    return Err(Error::from(e))
                }
            };
            used_memory += arg_value.get_memory_use();
            evaluated_args.push(arg_value);
        }
        let mut resp = match function {
            CallableType::NativeFunction(_, function, cost_function) => {
                let arg_size = evaluated_args.len();
                runtime_cost!(cost_function, env, arg_size)?;
                function.apply(evaluated_args)
            },
            CallableType::UserFunction(function) => function.apply(&evaluated_args, env),
            _ => panic!("Should be unreachable.")
        };
        add_stack_trace(&mut resp, env);
        env.drop_memory(used_memory);
        env.call_stack.remove(&identifier, track_recursion)?;
        resp
    }
}

pub fn eval <'a> (exp: &SymbolicExpression, env: &'a mut Environment, context: &LocalContext) -> Result<Value> {
    use vm::representations::SymbolicExpressionType::{AtomValue, Atom, List, LiteralValue, TraitReference, Field};

    match exp.expr {
        AtomValue(ref value) | LiteralValue(ref value) => Ok(value.clone()),
        Atom(ref value) => lookup_variable(&value, context, env),
        List(ref children) => {
            let (function_variable, rest) = children.split_first()
                .ok_or(CheckErrors::NonFunctionApplication)?;
            let function_name = function_variable.match_atom()
                .ok_or(CheckErrors::BadFunctionName)?;
            let f = lookup_function(&function_name, env)?;
            apply(&f, &rest, env, context)
        },
        TraitReference(_, _) | Field(_) => unreachable!("can't be evaluated"),
    }
}


pub fn is_reserved(name: &str) -> bool {
    if let Some(_result) = functions::lookup_reserved_functions(name) {
        true
    } else if variables::is_reserved_name(name) {
        true
    } else {
        false
    }
}

/* This function evaluates a list of expressions, sharing a global context.
 * It returns the final evaluated result.
 */
fn eval_all (expressions: &[SymbolicExpression],
             contract_context: &mut ContractContext,
             global_context: &mut GlobalContext) -> Result<Option<Value>> {
    let mut last_executed = None;
    let context = LocalContext::new();
    let mut total_memory_use = 0;

    let publisher = Value::Principal(
        PrincipalData::Standard(contract_context.contract_identifier.issuer.clone()));

    finally_drop_memory!(global_context, total_memory_use; {
        for exp in expressions {
            let try_define = global_context.execute(|context| {
                let mut call_stack = CallStack::new();
                let mut env = Environment::new(
                    context, contract_context, &mut call_stack, Some(publisher.clone()), Some(publisher.clone()));
                functions::define::evaluate_define(exp, &mut env)
            })?;
            match try_define {
                DefineResult::Variable(name, value) => {
                    runtime_cost!(cost_functions::BIND_NAME, global_context, 0)?;
                    let value_memory_use = value.get_memory_use();
                    global_context.add_memory(value_memory_use)?;
                    total_memory_use += value_memory_use;

                    contract_context.variables.insert(name, value);
                },
                DefineResult::Function(name, value) => {
                    runtime_cost!(cost_functions::BIND_NAME, global_context, 0)?;

                    contract_context.functions.insert(name, value);
                },
                DefineResult::PersistedVariable(name, value_type, value) => {
                    runtime_cost!(cost_functions::CREATE_VAR, global_context, value_type.size())?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(value_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    global_context.add_memory(value.size() as u64)?;

                    global_context.database.create_variable(&contract_context.contract_identifier, &name, value_type);
                    global_context.database.set_variable(&contract_context.contract_identifier, &name, value)?;
                },
                DefineResult::Map(name, key_type, value_type) => {
                    runtime_cost!(cost_functions::CREATE_MAP, global_context,
                                  u64::from(key_type.size()).cost_overflow_add(
                                      u64::from(value_type.size()))?)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(key_type.type_size()
                                              .expect("type size should be realizable") as u64)?;
                    global_context.add_memory(value_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    global_context.database.create_map(&contract_context.contract_identifier, &name, key_type, value_type);
                },
                DefineResult::FungibleToken(name, total_supply) => {
                    runtime_cost!(cost_functions::CREATE_FT, global_context, 0)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(TypeSignature::UIntType.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    global_context.database.create_fungible_token(&contract_context.contract_identifier, &name, &total_supply);
                },
                DefineResult::NonFungibleAsset(name, asset_type) => {
                    runtime_cost!(cost_functions::CREATE_NFT, global_context, asset_type.size())?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(asset_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    global_context.database.create_non_fungible_token(&contract_context.contract_identifier, &name, &asset_type);
                },
                DefineResult::Trait(name, trait_type) => { 
                    contract_context.defined_traits.insert(name, trait_type);
                },
                DefineResult::UseTrait(_name, _trait_identifier) => {},
                DefineResult::ImplTrait(trait_identifier) => {
                    contract_context.implemented_traits.insert(trait_identifier);
                },
                DefineResult::NoDefine => {
                    // not a define function, evaluate normally.
                    global_context.execute(|global_context| {
                        let mut call_stack = CallStack::new();
                        let mut env = Environment::new(
                            global_context, contract_context, &mut call_stack, Some(publisher.clone()), Some(publisher.clone()));

                        let result = eval(exp, &mut env, &context)?;
                        last_executed = Some(result);
                        Ok(())
                    })?;
                }
            }
        }

        contract_context.data_size = total_memory_use;
        Ok(last_executed)
    })
}

/* Run provided program in a brand new environment, with a transient, empty
 *  database.
 *
 *  Only used by CLI.
 */
pub fn execute(program: &str) -> Result<Option<Value>> {
    let contract_id = QualifiedContractIdentifier::transient();
    let mut contract_context = ContractContext::new(contract_id.clone());
    let mut marf = MemoryBackingStore::new();
    let conn = marf.as_clarity_db();
    let mut global_context = GlobalContext::new(conn, LimitedCostTracker::new_max_limit());
    global_context.execute(|g| {
        let parsed = ast::build_ast(&contract_id, program, &mut ())?
            .expressions;
        eval_all(&parsed, &mut contract_context, g)
    })
}


#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use vm::database::{MemoryBackingStore};
    use vm::{Value, LocalContext, GlobalContext, ContractContext, Environment, SymbolicExpression, CallStack};
    use vm::types::{TypeSignature, QualifiedContractIdentifier};
    use vm::callables::{DefinedFunction, DefineType};
    use vm::eval;
    use vm::costs::LimitedCostTracker;
    use vm::execute;
    use vm::errors::RuntimeErrorType;

    #[test]
    fn test_simple_user_function() {
        //
        //  test program:
        //  (define (do_work x) (+ 5 x))
        //  (define a 59)
        //  (do_work a)
        //
        let content = [ SymbolicExpression::list(
            Box::new([ SymbolicExpression::atom("do_work".into()),
                       SymbolicExpression::atom("a".into()) ])) ];

        let func_body = SymbolicExpression::list(
            Box::new([ SymbolicExpression::atom("+".into()),
                       SymbolicExpression::atom_value(Value::Int(5)),
                       SymbolicExpression::atom("x".into())]));

        let func_args = vec![("x".into(), TypeSignature::IntType)];
        let user_function = DefinedFunction::new(func_args, 
                                                 func_body, 
                                                 DefineType::Private,
                                                 &"do_work".into(), 
                                                 &"");

        let context = LocalContext::new();
        let mut contract_context = ContractContext::new(QualifiedContractIdentifier::transient());

        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(marf.as_clarity_db(), LimitedCostTracker::new_max_limit());

        contract_context.variables.insert("a".into(), Value::Int(59));
        contract_context.functions.insert("do_work".into(), user_function);

        let mut call_stack = CallStack::new();
        let mut env = Environment::new(&mut global_context, &contract_context, &mut call_stack, None, None);
        assert_eq!(Ok(Value::Int(64)), eval(&content[0], &mut env, &context));
    }
}
