pub mod define;
pub mod tuples;
mod sequences;
mod arithmetic;
mod boolean;
mod database;
mod options;
mod assets;
mod special;

use vm::errors::{Error, CheckErrors, RuntimeErrorType, ShortReturnType, InterpreterResult as Result, check_argument_count, check_arguments_at_least};
use vm::types::{Value, SequenceData, CharType, PrincipalData, ResponseData, TypeSignature};
use vm::callables::{CallableType, NativeHandle};
use vm::representations::{SymbolicExpression, SymbolicExpressionType, ClarityName};
use vm::representations::SymbolicExpressionType::{List, Atom};
use vm::{LocalContext, Environment, eval};
use vm::costs::{cost_functions, MemoryConsumer, CostTracker, constants as cost_constants};
use util::hash;

pub use vm::functions::assets::{get_stx_balance_snapshot, stx_transfer_consolidated};

pub use vm::functions::special::handle_contract_call_special_cases;

define_named_enum!(NativeFunctions {
    Add("+"),
    Subtract("-"),
    Multiply("*"),
    Divide("/"),
    CmpGeq(">="),
    CmpLeq("<="),
    CmpLess("<"),
    CmpGreater(">"),
    ToInt("to-int"),
    ToUInt("to-uint"),
    Modulo("mod"),
    Power("pow"),
    BitwiseXOR("xor"),
    And("and"),
    Or("or"),
    Not("not"),
    Equals("is-eq"),
    If("if"),
    Let("let"),
    Map("map"),
    Fold("fold"),
    Append("append"),
    Concat("concat"),
    AsMaxLen("as-max-len?"),
    Len("len"),
    ListCons("list"),
    FetchVar("var-get"),
    SetVar("var-set"),
    FetchEntry("map-get?"),
    SetEntry("map-set"),
    InsertEntry("map-insert"),
    DeleteEntry("map-delete"),
    TupleCons("tuple"),
    TupleGet("get"),
    Begin("begin"),
    Hash160("hash160"),
    Sha256("sha256"),
    Sha512("sha512"),
    Sha512Trunc256("sha512/256"),
    Keccak256("keccak256"),
    Print("print"),
    ContractCall("contract-call?"),
    AsContract("as-contract"),
    ContractOf("contract-of"),
    AtBlock("at-block"),
    GetBlockInfo("get-block-info?"),
    ConsError("err"),
    ConsOkay("ok"),
    ConsSome("some"),
    DefaultTo("default-to"),
    Asserts("asserts!"),
    UnwrapRet("unwrap!"),
    UnwrapErrRet("unwrap-err!"),
    Unwrap("unwrap-panic"),
    UnwrapErr("unwrap-err-panic"),
    Match("match"),
    TryRet("try!"),
    IsOkay("is-ok"),
    IsNone("is-none"),
    IsErr("is-err"),
    IsSome("is-some"),
    Filter("filter"),
    GetTokenBalance("ft-get-balance"),
    GetAssetOwner("nft-get-owner?"),
    TransferToken("ft-transfer?"),
    TransferAsset("nft-transfer?"),
    MintAsset("nft-mint?"),
    MintToken("ft-mint?"),
    GetStxBalance("stx-get-balance"),
    StxTransfer("stx-transfer?"),
    StxBurn("stx-burn?"),
});

pub fn lookup_reserved_functions(name: &str) -> Option<CallableType> {
    use vm::functions::NativeFunctions::*;
    use vm::callables::CallableType::{ NativeFunction, SpecialFunction };
    if let Some(native_function) = NativeFunctions::lookup_by_name(name) {
        let callable = match native_function {
            Add => NativeFunction("native_add", NativeHandle::MoreArg(&arithmetic::native_add), cost_functions::ADD),
            Subtract => NativeFunction("native_sub", NativeHandle::MoreArg(&arithmetic::native_sub), cost_functions::SUB),
            Multiply => NativeFunction("native_mul", NativeHandle::MoreArg(&arithmetic::native_mul), cost_functions::MUL),
            Divide => NativeFunction("native_div", NativeHandle::MoreArg(&arithmetic::native_div), cost_functions::DIV),
            CmpGeq => NativeFunction("native_geq", NativeHandle::DoubleArg(&arithmetic::native_geq), cost_functions::GEQ),
            CmpLeq => NativeFunction("native_leq", NativeHandle::DoubleArg(&arithmetic::native_leq), cost_functions::LEQ),
            CmpLess => NativeFunction("native_le", NativeHandle::DoubleArg(&arithmetic::native_le), cost_functions::LE),
            CmpGreater => NativeFunction("native_ge", NativeHandle::DoubleArg(&arithmetic::native_ge), cost_functions::GE),
            ToUInt => NativeFunction("native_to_uint", NativeHandle::SingleArg(&arithmetic::native_to_uint), cost_functions::INT_CAST),
            ToInt => NativeFunction("native_to_int", NativeHandle::SingleArg(&arithmetic::native_to_int), cost_functions::INT_CAST),
            Modulo => NativeFunction("native_mod", NativeHandle::DoubleArg(&arithmetic::native_mod), cost_functions::MOD),
            Power => NativeFunction("native_pow", NativeHandle::DoubleArg(&arithmetic::native_pow), cost_functions::POW),
            BitwiseXOR => NativeFunction("native_xor", NativeHandle::DoubleArg(&arithmetic::native_xor), cost_functions::XOR),
            And => SpecialFunction("special_and", &boolean::special_and),
            Or => SpecialFunction("special_or", &boolean::special_or),
            Not => NativeFunction("native_not", NativeHandle::SingleArg(&boolean::native_not), cost_functions::NOT),
            Equals => NativeFunction("native_eq", NativeHandle::MoreArg(&native_eq), cost_functions::EQ),
            If => SpecialFunction("special_if", &special_if),
            Let => SpecialFunction("special_let", &special_let),
            FetchVar => SpecialFunction("special_var-get", &database::special_fetch_variable),
            SetVar => SpecialFunction("special_set-var", &database::special_set_variable),
            Map => SpecialFunction("special_map", &sequences::special_map),
            Filter => SpecialFunction("special_filter", &sequences::special_filter),
            Fold => SpecialFunction("special_fold", &sequences::special_fold),
            Concat => SpecialFunction("special_concat", &sequences::special_concat),
            AsMaxLen => SpecialFunction("special_as_max_len", &sequences::special_as_max_len),
            Append => SpecialFunction("special_append", &sequences::special_append),
            Len => NativeFunction("native_len", NativeHandle::SingleArg(&sequences::native_len), cost_functions::LEN),
            ListCons => SpecialFunction("special_list_cons", &sequences::list_cons),
            FetchEntry => SpecialFunction("special_map-get?", &database::special_fetch_entry),
            SetEntry => SpecialFunction("special_set-entry", &database::special_set_entry),
            InsertEntry => SpecialFunction("special_insert-entry", &database::special_insert_entry),
            DeleteEntry => SpecialFunction("special_delete-entry", &database::special_delete_entry),
            TupleCons => SpecialFunction("special_tuple", &tuples::tuple_cons),
            TupleGet => SpecialFunction("special_get-tuple", &tuples::tuple_get),
            Begin => NativeFunction("native_begin", NativeHandle::MoreArg(&native_begin), cost_functions::BEGIN),
            Hash160 => NativeFunction("native_hash160", NativeHandle::SingleArg(&native_hash160), cost_functions::HASH160),
            Sha256 => NativeFunction("native_sha256", NativeHandle::SingleArg(&native_sha256), cost_functions::SHA256),
            Sha512 => NativeFunction("native_sha512", NativeHandle::SingleArg(&native_sha512), cost_functions::SHA512),
            Sha512Trunc256 => NativeFunction("native_sha512trunc256", NativeHandle::SingleArg(&native_sha512trunc256), cost_functions::SHA512T256),
            Keccak256 => NativeFunction("native_keccak256", NativeHandle::SingleArg(&native_keccak256), cost_functions::KECCAK256),
            Print => SpecialFunction("special_print", &special_print),
            ContractCall => SpecialFunction("special_contract-call", &database::special_contract_call),
            AsContract => SpecialFunction("special_as-contract", &special_as_contract),
            ContractOf => SpecialFunction("special_contract-of", &special_contract_of),
            GetBlockInfo => SpecialFunction("special_get_block_info", &database::special_get_block_info),
            ConsSome => NativeFunction("native_some", NativeHandle::SingleArg(&options::native_some), cost_functions::SOME_CONS),
            ConsOkay => NativeFunction("native_okay", NativeHandle::SingleArg(&options::native_okay), cost_functions::OK_CONS),
            ConsError => NativeFunction("native_error", NativeHandle::SingleArg(&options::native_error), cost_functions::ERR_CONS),
            DefaultTo => NativeFunction("native_default_to", NativeHandle::DoubleArg(&options::native_default_to), cost_functions::DEFAULT_TO),
            Asserts => SpecialFunction("special_asserts", &special_asserts),
            UnwrapRet => NativeFunction("native_unwrap_ret", NativeHandle::DoubleArg(&options::native_unwrap_or_ret), cost_functions::UNWRAP_RET),
            UnwrapErrRet => NativeFunction("native_unwrap_err_ret", NativeHandle::DoubleArg(&options::native_unwrap_err_or_ret), cost_functions::UNWRAP_ERR_OR_RET),
            IsOkay => NativeFunction("native_is_okay", NativeHandle::SingleArg(&options::native_is_okay), cost_functions::IS_OKAY),
            IsNone => NativeFunction("native_is_none", NativeHandle::SingleArg(&options::native_is_none), cost_functions::IS_NONE),
            IsErr => NativeFunction("native_is_err", NativeHandle::SingleArg(&options::native_is_err), cost_functions::IS_ERR),
            IsSome => NativeFunction("native_is_some", NativeHandle::SingleArg(&options::native_is_some), cost_functions::IS_SOME),
            Unwrap => NativeFunction("native_unwrap", NativeHandle::SingleArg(&options::native_unwrap), cost_functions::UNWRAP),
            UnwrapErr => NativeFunction("native_unwrap_err", NativeHandle::SingleArg(&options::native_unwrap_err), cost_functions::UNWRAP_ERR),
            Match => SpecialFunction("special_match", &options::special_match),
            TryRet => NativeFunction("native_try_ret", NativeHandle::SingleArg(&options::native_try_ret), cost_functions::TRY_RET),
            MintAsset => SpecialFunction("special_mint_asset", &assets::special_mint_asset),
            MintToken => SpecialFunction("special_mint_token", &assets::special_mint_token),
            TransferAsset => SpecialFunction("special_transfer_asset", &assets::special_transfer_asset),
            TransferToken => SpecialFunction("special_transfer_token", &assets::special_transfer_token),
            GetTokenBalance => SpecialFunction("special_get_balance", &assets::special_get_balance),
            GetAssetOwner => SpecialFunction("special_get_owner", &assets::special_get_owner),
            AtBlock => SpecialFunction("special_at_block", &database::special_at_block),
            GetStxBalance => SpecialFunction("special_stx_balance", &assets::special_stx_balance),
            StxTransfer => SpecialFunction("special_stx_transfer", &assets::special_stx_transfer),
            StxBurn => SpecialFunction("special_stx_burn", &assets::special_stx_burn),
        };
        Some(callable)
    } else {
        None
    }
}

fn native_eq(args: Vec<Value>) -> Result<Value> {
    // TODO: this currently uses the derived equality checks of Value,
    //   however, that's probably not how we want to implement equality
    //   checks on the ::ListTypes

    if args.len() < 2 {
        Ok(Value::Bool(true))
    } else {
        let first = &args[0];
        // check types:
        let mut arg_type = TypeSignature::type_of(first);
        for x in args.iter() {
            arg_type = TypeSignature::least_supertype(&TypeSignature::type_of(x), &arg_type)?;
            if x != first {
                return Ok(Value::Bool(false))
            }
        }
        Ok(Value::Bool(true))
    }
}

macro_rules! native_hash_func {
    ($name:ident, $module:ty) => {
        fn $name(input: Value) -> Result<Value> {
            let bytes = match input {
                Value::Int(value) => Ok(value.to_le_bytes().to_vec()),
                Value::UInt(value) => Ok(value.to_le_bytes().to_vec()),
                Value::Sequence(SequenceData::Buffer(value)) => Ok(value.data),
                _ => Err(CheckErrors::UnionTypeValueError(vec![TypeSignature::IntType, TypeSignature::UIntType, TypeSignature::max_buffer()], input))
            }?;
            let hash = <$module>::from_data(&bytes);
            Value::buff_from(hash.as_bytes().to_vec())
        }
    }
}

native_hash_func!(native_hash160, hash::Hash160);
native_hash_func!(native_sha256, hash::Sha256Sum);
native_hash_func!(native_sha512, hash::Sha512Sum);
native_hash_func!(native_sha512trunc256, hash::Sha512Trunc256Sum);
native_hash_func!(native_keccak256, hash::Keccak256Hash);

fn native_begin(mut args: Vec<Value>) -> Result<Value> {
    match args.pop() {
        Some(v) => Ok(v),
        None => Err(CheckErrors::RequiresAtLeastArguments(1,0).into())
    }
}

fn special_print(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    let input = eval(&args[0], env, context)?;

    runtime_cost!(cost_functions::PRINT, env, input.size())?;

    if cfg!(feature = "developer-mode") {
        eprintln!("{}", &input);
    }

    env.register_print_event(input.clone())?;
    Ok(input)
}

fn special_if(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    check_argument_count(3, args)?;

    runtime_cost!(cost_functions::IF, env, 0)?;
    // handle the conditional clause.
    let conditional = eval(&args[0], env, context)?;
    match conditional {
        Value::Bool(result) => {
            if result {
                eval(&args[1], env, context)
            } else {
                eval(&args[2], env, context)
            }
        },
        _ => Err(CheckErrors::TypeValueError(TypeSignature::BoolType, conditional).into())
    }
}

fn special_asserts(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    check_argument_count(2, args)?;

    runtime_cost!(cost_functions::ASSERTS, env, 0)?;
    // handle the conditional clause.
    let conditional = eval(&args[0], env, context)?;

    match conditional {
        Value::Bool(result) => {
            if result {
                Ok(conditional)
            } else {
                let thrown = eval(&args[1], env, context)?;
                Err(ShortReturnType::AssertionFailed(thrown.clone()).into())
            }
        },
        _ => Err(CheckErrors::TypeValueError(TypeSignature::BoolType, conditional).into())
    }
}

pub fn handle_binding_list <F, E> (bindings: &[SymbolicExpression], mut handler: F) -> std::result::Result<(), E>
where F: FnMut(&ClarityName, &SymbolicExpression) -> std::result::Result<(), E>,
      E: From<CheckErrors>
{
    for binding in bindings.iter() {
        let binding_expression = binding.match_list()
            .ok_or(CheckErrors::BadSyntaxBinding)?;
        if binding_expression.len() != 2 {
            return Err(CheckErrors::BadSyntaxBinding.into());
        }
        let var_name = binding_expression[0].match_atom()
            .ok_or(CheckErrors::BadSyntaxBinding)?;
        let var_sexp = &binding_expression[1];

        handler(var_name, var_sexp)?;
    }
    Ok(())
}

pub fn parse_eval_bindings(bindings: &[SymbolicExpression],
                       env: &mut Environment, context: &LocalContext)-> Result<Vec<(ClarityName, Value)>> {
    let mut result = Vec::new();
    handle_binding_list(bindings, |var_name, var_sexp| {
        eval(var_sexp, env, context)
            .and_then(|value| {
                result.push((var_name.clone(), value));
                Ok(()) })
    })?;

    Ok(result)
}

fn special_let(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    use vm::is_reserved;

    // (let ((x 1) (y 2)) (+ x y)) -> 3
    // arg0 => binding list
    // arg1..n => body
    check_arguments_at_least(2, args)?;

    // parse and eval the bindings.
    let bindings = args[0].match_list()
        .ok_or(CheckErrors::BadLetSyntax)?;

    runtime_cost!(cost_functions::LET, env, bindings.len())?;

    // create a new context.
    let mut inner_context = context.extend()?;

    let mut memory_use = 0;

    finally_drop_memory!( env, memory_use; {
        handle_binding_list::<_, Error>(bindings, |binding_name, var_sexp| {
            if is_reserved(binding_name) ||
                env.contract_context.lookup_function(binding_name).is_some() ||
                inner_context.lookup_variable(binding_name).is_some() {
                    return Err(CheckErrors::NameAlreadyUsed(binding_name.clone().into()).into())
                }

            let binding_value = eval(var_sexp, env, context)?;

            let bind_mem_use = binding_value.get_memory_use();
            env.add_memory(bind_mem_use)?;
            memory_use += bind_mem_use; // no check needed, b/c it's done in add_memory.
            inner_context.variables.insert(binding_name.clone(), binding_value);
            Ok(())
        })?;

        // evaluate the let-bodies
        let mut last_result = None;
        for body in args[1..].iter() {
            let body_result = eval(&body, env, &inner_context)?;
            last_result.replace(body_result);
        }
        // last_result should always be Some(...), because of the arg len check above.
        Ok(last_result.unwrap())
    })
}

fn special_as_contract(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    // (as-contract (..))
    // arg0 => body
    check_argument_count(1, args)?;

    // nest an environment.
    env.add_memory(cost_constants::AS_CONTRACT_MEMORY)?;

    let contract_principal = Value::Principal(PrincipalData::Contract(env.contract_context.contract_identifier.clone()));
    let mut nested_env = env.nest_as_principal(contract_principal);

    let result = eval(&args[0], &mut nested_env, context);

    env.drop_memory(cost_constants::AS_CONTRACT_MEMORY);

    result
}

fn special_contract_of(args: &[SymbolicExpression], env: &mut Environment, context: &LocalContext) -> Result<Value> {
    // (contract-of (..))
    // arg0 => trait
    check_argument_count(1, args)?;

    runtime_cost!(cost_functions::CONTRACT_OF, env, 0)?;

    let contract_ref = match &args[0].expr {
        SymbolicExpressionType::Atom(contract_ref) => {
            contract_ref
        },
        _ => return Err(CheckErrors::ContractOfExpectsTrait.into())
    };

    let contract_identifier = match context.lookup_callable_contract(contract_ref) {
        Some((ref contract_identifier, _trait_identifier)) => {
            env.global_context.database.get_contract(contract_identifier)
                .map_err(|_e| CheckErrors::NoSuchContract(contract_identifier.to_string()))?;

            contract_identifier
        },
        _ => return Err(CheckErrors::ContractOfExpectsTrait.into())
    };

    let contract_principal = Value::Principal(PrincipalData::Contract(contract_identifier.clone()));
    Ok(contract_principal)
}
