use candid::{candid_method, Nat, Principal};
use ic_cdk::trap;
use ic_cdk_macros::{init, query, update};
use ic_cdk_timers::TimerId;
use ic_crypto_sha::Sha256;
use ic_icrc1::blocks::{encoded_block_to_generic_block, generic_block_to_encoded_block};
use ic_icrc1::{Block, Operation};
use ic_icrc1_index_ng::{
    GetAccountTransactionsArgs, GetAccountTransactionsResponse, GetAccountTransactionsResult,
    IndexArg, TransactionWithId,
};
use ic_ledger_core::block::{BlockIndex as LedgerBlockIndex, BlockType, EncodedBlock};
use ic_stable_structures::memory_manager::{MemoryId, VirtualMemory};
use ic_stable_structures::{
    memory_manager::MemoryManager, DefaultMemoryImpl, StableBTreeMap, StableCell, StableLog,
    Storable,
};
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc3::archive::{ArchivedRange, QueryBlockArchiveFn};
use icrc_ledger_types::icrc3::blocks::{
    BlockRange, GenericBlock, GetBlocksRequest, GetBlocksResponse,
};
use icrc_ledger_types::icrc3::transactions::{Burn, Mint, Transaction, Transfer};
use num_traits::ToPrimitive;
use scopeguard::{guard, ScopeGuard};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::hash::Hash;
use std::time::Duration;

/// The maximum number of blocks to return in a single [get_blocks] request.
const DEFAULT_MAX_BLOCKS_PER_RESPONSE: u64 = 2000;

const STATE_MEMORY_ID: MemoryId = MemoryId::new(0);
const BLOCK_LOG_INDEX_MEMORY_ID: MemoryId = MemoryId::new(1);
const BLOCK_LOG_DATA_MEMORY_ID: MemoryId = MemoryId::new(2);
const ACCOUNT_BLOCK_IDS_MEMORY_ID: MemoryId = MemoryId::new(3);

const DEFAULT_MAX_WAIT_TIME: Duration = Duration::from_secs(60);
const DEFAULT_RETRY_WAIT_TIME: Duration = Duration::from_secs(10);

type VM = VirtualMemory<DefaultMemoryImpl>;
type StateCell = StableCell<State, VM>;
type BlockLog = StableLog<Vec<u8>, VM, VM>;
// The block indexes are stored in reverse order because the blocks/transactions
// are returned in reversed order.
type AccountBlockIdsMapKey = ([u8; Sha256::DIGEST_LEN], Reverse<u64>);
type AccountBlockIdsMap = StableBTreeMap<AccountBlockIdsMapKey, (), VM>;

thread_local! {
    /// Static memory manager to manage the memory available for stable structures.
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> = RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));

    /// Scalar state of the index.
    static STATE: RefCell<StateCell> = with_memory_manager(|memory_manager| {
        RefCell::new(StateCell::init(memory_manager.get(STATE_MEMORY_ID), State::default())
            .expect("failed to initialize stable cell"))
    });

    /// Append-only list of encoded blocks stored in stable memory.
    static BLOCKS: RefCell<BlockLog> = with_memory_manager(|memory_manager| {
        RefCell::new(BlockLog::init(memory_manager.get(BLOCK_LOG_INDEX_MEMORY_ID), memory_manager.get(BLOCK_LOG_DATA_MEMORY_ID))
            .expect("failed to initialize stable log"))
    });

    /// Map that contains the block ids of an account.
    /// The account is hashed to save space.
    static ACCOUNT_BLOCK_IDS: RefCell<AccountBlockIdsMap> = with_memory_manager(|memory_manager| {
        RefCell::new(AccountBlockIdsMap::init(memory_manager.get(ACCOUNT_BLOCK_IDS_MEMORY_ID)))
    });
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct State {
    // Equals to `true` while the [build_index] task runs.
    is_build_index_running: bool,

    /// The principal of the ledger canister that is indexed by this index.
    ledger_id: Principal,

    /// The maximum number of transactions returned by [get_blocks].
    max_blocks_per_response: u64,
}

// NOTE: the default configuration is dysfunctional, but it's convenient to have
// a Default impl for the initialization of the [STATE] variable above.
impl Default for State {
    fn default() -> Self {
        Self {
            is_build_index_running: false,
            ledger_id: Principal::management_canister(),
            max_blocks_per_response: DEFAULT_MAX_BLOCKS_PER_RESPONSE,
        }
    }
}

impl Storable for State {
    fn to_bytes(&self) -> Cow<[u8]> {
        let mut buf = vec![];
        ciborium::ser::into_writer(self, &mut buf).expect("failed to encode index config");
        Cow::Owned(buf)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        ciborium::de::from_reader(&bytes[..]).expect("failed to decode index options")
    }
}

/// A helper function to access the scalar state.
fn with_state<R>(f: impl FnOnce(&State) -> R) -> R {
    STATE.with(|cell| f(cell.borrow().get()))
}

/// A helper function to change the scalar state.
fn change_state(f: impl FnOnce(&mut State)) {
    STATE
        .with(|cell| {
            let mut borrowed = cell.borrow_mut();
            let mut state = *borrowed.get();
            f(&mut state);
            borrowed.set(state)
        })
        .expect("failed to set index state");
}

/// A helper function to access the memory manager.
fn with_memory_manager<R>(f: impl FnOnce(&MemoryManager<DefaultMemoryImpl>) -> R) -> R {
    MEMORY_MANAGER.with(|cell| f(&cell.borrow()))
}

/// A helper function to access the block list.
fn with_blocks<R>(f: impl FnOnce(&BlockLog) -> R) -> R {
    BLOCKS.with(|cell| f(&cell.borrow()))
}

/// A helper function to access the account block ids.
fn with_account_block_ids<R>(f: impl FnOnce(&mut AccountBlockIdsMap) -> R) -> R {
    ACCOUNT_BLOCK_IDS.with(|cell| f(&mut cell.borrow_mut()))
}

fn with_blocks_and_indices<R>(f: impl FnOnce(&BlockLog, &mut AccountBlockIdsMap) -> R) -> R {
    with_blocks(|blocks| with_account_block_ids(|account_block_ids| f(blocks, account_block_ids)))
}

#[init]
#[candid_method(init)]
fn init(index_arg: Option<IndexArg>) {
    let init_arg = match index_arg {
        Some(IndexArg::InitArg(arg)) => arg,
        _ => trap("Index initialization must take in input an InitArg argument"),
    };

    // stable memory initialization
    change_state(|state| {
        state.ledger_id = init_arg.ledger_id;
    });

    // set the first build_index to be called after init
    set_build_index_timer(Duration::from_secs(1));
}

async fn get_blocks_from_ledger(start: u64) -> Result<GetBlocksResponse, String> {
    let (ledger_id, length) = with_state(|state| (state.ledger_id, state.max_blocks_per_response));
    let req = GetBlocksRequest {
        start: Nat::from(start),
        length: Nat::from(length),
    };
    let (res,): (GetBlocksResponse,) = ic_cdk::call(ledger_id, "get_blocks", (req,))
        .await
        .map_err(|(code, str)| format!("code: {:#?} message: {}", code, str))?;
    Ok(res)
}

async fn get_blocks_from_archive(
    archived: &ArchivedRange<QueryBlockArchiveFn>,
) -> Result<BlockRange, String> {
    let req = GetBlocksRequest {
        start: archived.start.clone(),
        length: archived.length.clone(),
    };
    let (res,): (BlockRange,) = ic_cdk::call(
        archived.callback.canister_id,
        &archived.callback.method,
        (req,),
    )
    .await
    .map_err(|(code, str)| format!("code: {:#?} message: {}", code, str))?;
    Ok(res)
}

pub async fn build_index() -> Result<(), String> {
    if with_state(|state| state.is_build_index_running) {
        return Err("build_index already running".to_string());
    }
    change_state(|state| {
        state.is_build_index_running = true;
    });
    let _reset_is_build_index_running_flag_guard = guard((), |_| {
        change_state(|state| {
            state.is_build_index_running = false;
        });
    });
    let failure_guard = guard((), |_| {
        set_build_index_timer(DEFAULT_RETRY_WAIT_TIME);
    });
    let next_txid = with_blocks(|blocks| blocks.len());
    let res = get_blocks_from_ledger(next_txid).await?;
    let mut tx_indexed_count: usize = 0;
    for archived in res.archived_blocks {
        let mut remaining = archived.length.clone();
        let mut next_archived_txid = archived.start.clone();
        while remaining > 0u32 {
            let archived = ArchivedRange::<QueryBlockArchiveFn> {
                start: next_archived_txid.clone(),
                length: remaining.clone(),
                callback: archived.callback.clone(),
            };
            let res = get_blocks_from_archive(&archived).await?;
            next_archived_txid += res.blocks.len();
            tx_indexed_count += res.blocks.len();
            remaining -= res.blocks.len();
            append_blocks(res.blocks);
        }
    }
    tx_indexed_count += res.blocks.len();
    append_blocks(res.blocks);
    let wait_time = compute_wait_time(tx_indexed_count);
    ic_cdk::eprintln!("Indexed: {} waiting : {:?}", tx_indexed_count, wait_time);
    ScopeGuard::into_inner(failure_guard);
    set_build_index_timer(wait_time);
    Ok(())
}

fn set_build_index_timer(after: Duration) -> TimerId {
    ic_cdk_timers::set_timer(after, || {
        ic_cdk::spawn(async {
            let _ = build_index().await;
        })
    })
}

/// Compute the waiting time before next indexing
pub fn compute_wait_time(indexed_tx_count: usize) -> Duration {
    let max_blocks_per_response = with_state(|state| state.max_blocks_per_response);
    if indexed_tx_count as u64 >= max_blocks_per_response {
        // If we indexed more than max_blocks_per_response,
        // we index again on the next build_index call.
        return Duration::ZERO;
    }
    let numerator = 1f64 - (indexed_tx_count as f64 / max_blocks_per_response as f64);
    DEFAULT_MAX_WAIT_TIME * (100f64 * numerator) as u32 / 100
}

fn append_blocks(new_blocks: Vec<GenericBlock>) {
    with_blocks_and_indices(|blocks, account_block_ids| {
        // the index of the next block that we
        // are going to append
        let mut block_index = blocks.len();
        for block in new_blocks {
            let block = generic_block_to_encoded_block_or_trap(block_index, block);

            // append the encoded block to the block log
            blocks
                .append(&block.0)
                .unwrap_or_else(|_| trap("no space left"));

            // add the block idx to the indices
            let decoded_block = decode_encoded_block_or_trap(block_index, block);
            for account in get_accounts(decoded_block) {
                account_block_ids.insert(account_block_ids_key(account, block_index), ());
            }

            block_index += 1;
        }
    });
}

fn generic_block_to_encoded_block_or_trap(
    block_index: LedgerBlockIndex,
    block: GenericBlock,
) -> EncodedBlock {
    generic_block_to_encoded_block(block).unwrap_or_else(|e| {
        trap(&format!(
            "Unable to decode generic block at index {}. Error: {}",
            block_index, e
        ))
    })
}

fn decode_encoded_block_or_trap(block_index: LedgerBlockIndex, block: EncodedBlock) -> Block {
    Block::decode(block).unwrap_or_else(|e| {
        trap(&format!(
            "Unable to decode encoded block at index {}. Error: {}",
            block_index, e
        ))
    })
}

fn get_accounts(block: Block) -> Vec<Account> {
    match block.transaction.operation {
        Operation::Burn { from, .. } => vec![from],
        Operation::Mint { to, .. } => vec![to],
        Operation::Transfer { from, to, .. } => vec![from, to],
    }
}

pub fn account_sha256(account: Account) -> [u8; Sha256::DIGEST_LEN] {
    let mut hasher = Sha256::new();
    account.hash(&mut hasher);
    hasher.finish()
}

fn account_block_ids_key(account: Account, block_index: LedgerBlockIndex) -> AccountBlockIdsMapKey {
    (account_sha256(account), Reverse(block_index))
}

fn decode_icrc1_block(_txid: u64, bytes: Vec<u8>) -> GenericBlock {
    let encoded_block = EncodedBlock::from(bytes);
    encoded_block_to_generic_block(&encoded_block)
}

#[query]
#[candid_method(query)]
fn get_blocks(req: GetBlocksRequest) -> ic_icrc1_index_ng::GetBlocksResponse {
    let chain_length = with_blocks(|blocks| blocks.len());
    let (start, length) = req
        .as_start_and_length()
        .unwrap_or_else(|msg| ic_cdk::api::trap(&msg));

    let blocks = decode_block_range(start, length, decode_icrc1_block);
    ic_icrc1_index_ng::GetBlocksResponse {
        chain_length,
        blocks,
    }
}

fn decode_block_range<R>(start: u64, length: u64, decoder: impl Fn(u64, Vec<u8>) -> R) -> Vec<R> {
    let length = length.min(with_state(|opts| opts.max_blocks_per_response));
    with_blocks(|blocks| {
        let limit = blocks.len().min(start.saturating_add(length));
        (start..limit)
            .map(|i| decoder(start + i, blocks.get(i).unwrap()))
            .collect()
    })
}

#[query]
#[candid_method(query)]
fn ledger_id() -> Principal {
    with_state(|state| state.ledger_id)
}

#[update]
#[candid_method(update)]
fn get_account_transactions(arg: GetAccountTransactionsArgs) -> GetAccountTransactionsResult {
    let length = arg
        .max_results
        .0
        .to_u64()
        .expect("The length must be a u64!")
        .min(with_state(|opts| opts.max_blocks_per_response))
        .min(usize::MAX as u64) as usize;
    // TODO: deal with the user setting start to u64::MAX
    let start = arg
        .start
        .map_or(u64::MAX, |n| n.0.to_u64().expect("start must be a u64!"));
    let key = account_block_ids_key(arg.account, start);
    let transactions = with_blocks_and_indices(|blocks, account_block_ids| {
        let mut transactions = vec![];
        let indices = account_block_ids
            .range(key..)
            // old txs of the requested account and skip the start index
            .filter(|(k, _)| k.0 == key.0 && k.1 .0 != start)
            .take(length)
            .map(|(k, _)| k.1);
        for id in indices {
            let block = blocks.get(id.0).unwrap_or_else(|| {
                trap(&format!(
                    "Block {} not found in the block log, account blocks map is corrupted!",
                    id.0
                ))
            });
            let transaction = encoded_block_bytes_to_flat_transaction(id.0, block);
            let transaction_with_idx = TransactionWithId {
                id: id.0.into(),
                transaction,
            };
            transactions.push(transaction_with_idx);
        }
        transactions
    });
    let oldest_tx_id = get_oldest_tx_id(arg.account).map(|tx_id| tx_id.into());
    Ok(GetAccountTransactionsResponse {
        transactions,
        oldest_tx_id,
    })
}

fn encoded_block_bytes_to_flat_transaction(
    block_index: LedgerBlockIndex,
    block: Vec<u8>,
) -> Transaction {
    let block = Block::decode(EncodedBlock::from(block)).unwrap_or_else(|e| {
        trap(&format!(
            "Unable to decode encoded block at index {}. Error: {}",
            block_index, e
        ))
    });
    let timestamp = block.timestamp;
    let created_at_time = block.transaction.created_at_time;
    let memo = block.transaction.memo;
    match block.transaction.operation {
        Operation::Burn { from, amount } => Transaction::burn(
            Burn {
                from,
                amount: amount.into(),
                created_at_time,
                memo,
            },
            timestamp,
        ),
        Operation::Mint { to, amount } => Transaction::mint(
            Mint {
                to,
                amount: amount.into(),
                created_at_time,
                memo,
            },
            timestamp,
        ),
        Operation::Transfer {
            from,
            to,
            amount,
            fee,
        } => Transaction::transfer(
            Transfer {
                from,
                to,
                amount: amount.into(),
                fee: fee.map(|fee| fee.into()),
                created_at_time,
                memo,
            },
            timestamp,
        ),
    }
}

fn get_oldest_tx_id(account: Account) -> Option<LedgerBlockIndex> {
    // TODO: there is no easy way to get the oldest_tx_id so we traverse
    //       all the transaction of the account for now. This will be
    //       fixed in future by storying the oldest_tx_id somewhere and
    //       replace the body of this function.
    let first_key = account_block_ids_key(account, u64::MAX);
    let last_key = account_block_ids_key(account, 0);
    with_account_block_ids(|account_block_ids| {
        account_block_ids
            .range(first_key..=last_key)
            .last()
            .map(|(k, _)| k.1 .0)
    })
}

fn main() {}

#[cfg(test)]
candid::export_service!();

#[test]
fn check_candid_interface() {
    use candid::utils::{service_compatible, CandidSource};
    use std::path::PathBuf;

    let new_interface = __export_service();
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let old_interface = manifest_dir.join("index-ng.did");
    service_compatible(
        CandidSource::Text(&new_interface),
        CandidSource::File(old_interface.as_path()),
    )
    .unwrap_or_else(|e| {
        panic!(
            "the index interface is not compatible with {}: {:?}",
            old_interface.display(),
            e
        )
    });
}

#[test]
fn compute_wait_time_test() {
    fn blocks(n: u64) -> usize {
        let max_blocks = DEFAULT_MAX_BLOCKS_PER_RESPONSE as f64;
        (max_blocks * n as f64 / 100f64) as usize
    }

    fn wait_time(n: u64) -> Duration {
        let max_wait_time = DEFAULT_MAX_WAIT_TIME.as_secs() as f64;
        Duration::from_secs((max_wait_time * n as f64 / 100f64) as u64)
    }

    assert_eq!(wait_time(100), compute_wait_time(blocks(0)));
    assert_eq!(wait_time(75), compute_wait_time(blocks(25)));
    assert_eq!(wait_time(50), compute_wait_time(blocks(50)));
    assert_eq!(wait_time(25), compute_wait_time(blocks(75)));
    assert_eq!(wait_time(0), compute_wait_time(blocks(100)));
}
