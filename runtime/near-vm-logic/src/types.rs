use serde::{Deserialize, Serialize};

pub type AccountId = String;
pub type PublicKey = Vec<u8>;
pub type BlockHeight = u64;
pub type Balance = u128;
pub type Gas = u64;
pub type PromiseIndex = u64;
pub type ReceiptIndex = u64;
pub type IteratorIndex = u64;
pub type StorageUsage = u64;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum ReturnData {
    #[serde(with = "crate::serde_with::bytes_as_str")]
    /// Method returned some value or data.
    Value(Vec<u8>),

    /// The return value of the method should be taken from the return value of another method
    /// identified through receipt index.
    ReceiptIndex(ReceiptIndex),

    /// Method hasn't returned any data or promise.
    None,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
/// When there is a callback attached to one or more contract calls the execution results of these
/// calls are available to the contract invoked through the callback.
pub enum PromiseResult {
    NotReady,
    #[serde(with = "crate::serde_with::bytes_as_str")]
    Successful(Vec<u8>),
    Failed,
}
