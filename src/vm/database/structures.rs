use vm::contracts::Contract;
use vm::errors::{Error, InterpreterError, RuntimeErrorType, InterpreterResult, IncomparableError};
use vm::types::{Value, OptionalData, TypeSignature, TupleTypeSignature, PrincipalData, NONE};

pub trait ClaritySerializable {
    fn serialize(&self) -> String;
}

pub trait ClarityDeserializable<T> {
    fn deserialize(json: &str) -> T;
}

impl ClaritySerializable for String {
    fn serialize(&self) -> String {
        self.into()
    }
}

impl ClarityDeserializable<String> for String {
    fn deserialize(serialized: &str) -> String {
        serialized.into()
    }
}

macro_rules! clarity_serializable {
    ($Name:ident) =>
    {
        impl ClaritySerializable for $Name {
            fn serialize(&self) -> String {
                serde_json::to_string(self)
                    .expect("Failed to serialize vm.Value")
            }
        }
        impl ClarityDeserializable<$Name> for $Name {
            fn deserialize(json: &str) -> Self {
                serde_json::from_str(json)
                    .expect("Failed to serialize vm.Value")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FungibleTokenMetadata {
    pub total_supply: Option<u128>
}

clarity_serializable!(FungibleTokenMetadata);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonFungibleTokenMetadata {
    pub key_type: TypeSignature
}

clarity_serializable!(NonFungibleTokenMetadata);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataMapMetadata {
    pub key_type: TypeSignature,
    pub value_type: TypeSignature
}

clarity_serializable!(DataMapMetadata);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataVariableMetadata {
    pub value_type: TypeSignature
}

clarity_serializable!(DataVariableMetadata);

#[derive(Serialize, Deserialize)]
pub struct ContractMetadata {
    pub contract: Contract
}

clarity_serializable!(ContractMetadata);


#[derive(Serialize, Deserialize)]
pub struct SimmedBlock {
    pub block_height: u64,
    pub block_time: u64,
    pub block_header_hash: [u8; 32],
    pub burn_chain_header_hash: [u8; 32],
    pub vrf_seed: [u8; 32],
}

clarity_serializable!(SimmedBlock);

clarity_serializable!(PrincipalData);
clarity_serializable!(i128);
clarity_serializable!(u128);
clarity_serializable!(u64);
clarity_serializable!(Contract);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct STXBalance {
    pub amount_unlocked: u128,
    pub amount_locked: u128,
    pub unlock_height: u64,
}
clarity_serializable!(STXBalance);

#[derive(Debug)]
pub enum STXBalanceError {
    Overflow,
    Underflow,
    LockActive,
}

type Result<T> = std::result::Result<T, STXBalanceError>;

impl STXBalance {

    pub fn zero() -> STXBalance {
        STXBalance {
            amount_unlocked: 0,
            amount_locked: 0,
            unlock_height: 0,
        }
    }

    pub fn initial(amount_unlocked: u128) -> STXBalance {
        STXBalance {
            amount_unlocked,
            amount_locked: 0,
            unlock_height: 0,
        }
    }

    pub fn get_available_balance_at_block(&self, block_height: u64) -> u128 {
        match self.has_locked_tokens_unlockable(block_height) {
            true => self.get_total_balance(),
            false => self.amount_unlocked
        }
    }

    pub fn lock_tokens(&mut self, amount_to_lock: u128, unlock_height: u64, current_height: u64) -> Result<()> {
        let unlocked = self.unlock_available_tokens_if_any(current_height);
        if  unlocked > 0 {
            debug!("Consolidated after account-token-lock");
        }

        if unlock_height <= current_height {
            panic!("FATAL: Can't set a lock with expired unlock_height");
        }

        if self.has_locked_tokens(current_height) {
            return Err(STXBalanceError::LockActive)
        }

        self.unlock_height = unlock_height;
        self.amount_unlocked = self.amount_unlocked.checked_sub(amount_to_lock)
            .expect("STX overflow");
        self.amount_locked = amount_to_lock;
        Ok(())
    }

    pub fn unlock_available_tokens_if_any(&mut self, block_height: u64) -> u128 {
        if !self.has_locked_tokens_unlockable(block_height) {
            return 0
        }

        let unlocked = self.amount_locked;
        self.unlock_height = 0;
        self.amount_unlocked = self.amount_unlocked.checked_add(unlocked)
            .expect("STX overflow");
        self.amount_locked = 0;
        unlocked
    }

    pub fn get_total_balance(&self) -> u128 {
        self.amount_unlocked.checked_add(self.amount_locked)
            .expect("STX overflow")
    }

    pub fn has_locked_tokens(&self, block_height: u64) -> bool {
        self.amount_locked > 0 && self.unlock_height > block_height
    }

    pub fn has_locked_tokens_unlockable(&self, block_height: u64) -> bool {
        self.amount_locked > 0 && self.unlock_height <= block_height
    }

    pub fn can_transfer(&self, amount: u128, block_height: u64) -> bool {
        self.get_available_balance_at_block(block_height) >= amount
    }

    pub fn debit(&mut self, amount: u128, block_height: u64) -> Result<()> {
        let unlocked = self.unlock_available_tokens_if_any(block_height);
        if  unlocked > 0 {
            debug!("Consolidated after account-debit");
        }

        self.amount_unlocked = self.amount_unlocked.checked_sub(amount)
            .ok_or_else(|| STXBalanceError::Underflow)?;
        Ok(())
    }

    pub fn credit(&mut self, amount: u128, block_height: u64) -> Result<()> {
        let unlocked = self.unlock_available_tokens_if_any(block_height);
        if  unlocked > 0 {
            debug!("Consolidated after account-credit");
        }

        self.amount_unlocked = self.amount_unlocked.checked_add(amount)
            .ok_or_else(|| STXBalanceError::Overflow)?;
        Ok(())
    }

    pub fn transfer_to(&mut self, recipient: &mut STXBalance, amount: u128, block_height: u64) -> Result<()> {
        self.debit(amount, block_height)?;
        recipient.credit(amount, block_height)?;
        Ok(())
    }
}
