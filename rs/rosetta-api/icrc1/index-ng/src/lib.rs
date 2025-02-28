use candid::{CandidType, Deserialize, Nat, Principal};
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc1::transfer::BlockIndex;
use icrc_ledger_types::icrc3::blocks::GenericBlock;
use icrc_ledger_types::icrc3::transactions::Transaction;

#[derive(CandidType, Debug, Deserialize)]
pub enum IndexArg {
    InitArg(InitArg),
}

#[derive(CandidType, Debug, Deserialize)]
pub struct InitArg {
    pub ledger_id: Principal,
}

#[derive(CandidType, Debug, Deserialize, Eq, PartialEq)]
pub struct GetBlocksResponse {
    // The length of the chain indexed.
    pub chain_length: u64,

    // The blocks in the requested range.
    pub blocks: Vec<GenericBlock>,
}

#[derive(CandidType, Debug, Deserialize, PartialEq, Eq)]
pub struct GetAccountTransactionsArgs {
    pub account: Account,
    // The txid of the last transaction seen by the client.
    // If None then the results will start from the most recent
    // txid. If set then the results will start from the next
    // most recent txid after start (start won't be included).
    pub start: Option<BlockIndex>,
    // Maximum number of transactions to fetch.
    pub max_results: Nat,
}

#[derive(CandidType, Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TransactionWithId {
    pub id: BlockIndex,
    pub transaction: Transaction,
}

#[derive(CandidType, Debug, Deserialize, PartialEq, Eq)]
pub struct GetAccountTransactionsResponse {
    pub transactions: Vec<TransactionWithId>,
    // The txid of the oldest transaction the account has
    pub oldest_tx_id: Option<BlockIndex>,
}

#[derive(CandidType, Debug, Deserialize, PartialEq, Eq)]
pub struct GetAccountTransactionsError {
    pub message: String,
}

pub type GetAccountTransactionsResult =
    Result<GetAccountTransactionsResponse, GetAccountTransactionsError>;
