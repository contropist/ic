use candid::{CandidType, Decode, Encode, Nat, Principal};
use ic_base_types::PrincipalId;
use ic_error_types::UserError;
use ic_icrc1::{endpoints::StandardRecord, hash::Hash, Block, Operation, Transaction};
use ic_ledger_canister_core::archive::ArchiveOptions;
use ic_ledger_core::block::{BlockIndex, BlockType, HashOf};
use ic_state_machine_tests::{CanisterId, ErrorCode, StateMachine, WasmResult};
use icrc_ledger_types::icrc::generic_metadata_value::MetadataValue as Value;
use icrc_ledger_types::icrc1::account::{Account, Subaccount};
use icrc_ledger_types::icrc1::transfer::{Memo, TransferArg, TransferError};
use icrc_ledger_types::icrc3::archive::ArchiveInfo;
use icrc_ledger_types::icrc3::blocks::BlockRange;
use icrc_ledger_types::icrc3::blocks::GenericBlock as IcrcBlock;
use icrc_ledger_types::icrc3::blocks::GetBlocksResponse;
use icrc_ledger_types::icrc3::transactions::GetTransactionsRequest;
use icrc_ledger_types::icrc3::transactions::GetTransactionsResponse;
use icrc_ledger_types::icrc3::transactions::Transaction as Tx;
use icrc_ledger_types::icrc3::transactions::TransactionRange;
use icrc_ledger_types::icrc3::transactions::Transfer;

use num_traits::ToPrimitive;
use proptest::prelude::*;
use proptest::test_runner::{Config as TestRunnerConfig, TestCaseResult, TestRunner};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, SystemTime},
};
pub const FEE: u64 = 10_000;
pub const ARCHIVE_TRIGGER_THRESHOLD: u64 = 10;
pub const NUM_BLOCKS_TO_ARCHIVE: u64 = 5;
pub const TX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

pub const MINTER: Account = Account {
    owner: PrincipalId::new(0, [0u8; 29]).0,
    subaccount: None,
};

// Metadata-related constants
pub const TOKEN_NAME: &str = "Test Token";
pub const TOKEN_SYMBOL: &str = "XTST";
pub const TEXT_META_KEY: &str = "test:image";
pub const TEXT_META_VALUE: &str = "grumpy_cat.png";
pub const TEXT_META_VALUE_2: &str = "dog.png";
pub const BLOB_META_KEY: &str = "test:blob";
pub const BLOB_META_VALUE: &[u8] = b"\xca\xfe\xba\xbe";
pub const NAT_META_KEY: &str = "test:nat";
pub const NAT_META_VALUE: u128 = u128::MAX;
pub const INT_META_KEY: &str = "test:int";
pub const INT_META_VALUE: i128 = i128::MIN;

#[derive(CandidType, Clone, Debug, PartialEq, Eq)]
pub struct InitArgs {
    pub minting_account: Account,
    pub fee_collector_account: Option<Account>,
    pub initial_balances: Vec<(Account, u64)>,
    pub transfer_fee: u64,
    pub token_name: String,
    pub token_symbol: String,
    pub metadata: Vec<(String, Value)>,
    pub archive_options: ArchiveOptions,
}

#[derive(CandidType, Clone, Debug, PartialEq, Eq)]
pub enum ChangeFeeCollector {
    Unset,
    SetTo(Account),
}

#[derive(CandidType, Clone, Debug, Default, PartialEq, Eq)]
pub struct UpgradeArgs {
    pub metadata: Option<Vec<(String, Value)>>,
    pub token_name: Option<String>,
    pub token_symbol: Option<String>,
    pub transfer_fee: Option<u64>,
    pub change_fee_collector: Option<ChangeFeeCollector>,
}

#[derive(CandidType, Clone, Debug, PartialEq, Eq)]
pub enum LedgerArgument {
    Init(InitArgs),
    Upgrade(Option<UpgradeArgs>),
}

fn test_transfer_model<T>(
    accounts: Vec<Account>,
    mints: Vec<u64>,
    transfers: Vec<(usize, usize, u64)>,
    ledger_wasm: Vec<u8>,
    encode_init_args: fn(InitArgs) -> T,
) -> TestCaseResult
where
    T: CandidType,
{
    let initial_balances: Vec<_> = mints
        .into_iter()
        .enumerate()
        .map(|(i, amount)| (accounts[i], amount))
        .collect();
    let mut balances: BalancesModel = initial_balances.iter().cloned().collect();

    let (env, canister_id) = setup(ledger_wasm, encode_init_args, initial_balances);

    for (from_idx, to_idx, amount) in transfers.into_iter() {
        let from = accounts[from_idx];
        let to = accounts[to_idx];

        let ((from_balance, to_balance), maybe_error) =
            model_transfer(&mut balances, from, to, amount);

        let result = transfer(&env, canister_id, from, to, amount);

        prop_assert_eq!(result.is_err(), maybe_error.is_some());

        if let Err(err) = result {
            prop_assert_eq!(Some(err), maybe_error);
        }

        let actual_from_balance = balance_of(&env, canister_id, from);
        let actual_to_balance = balance_of(&env, canister_id, to);

        prop_assert_eq!(from_balance, actual_from_balance);
        prop_assert_eq!(to_balance, actual_to_balance);
    }
    Ok(())
}

type BalancesModel = HashMap<Account, u64>;

fn model_transfer(
    balances: &mut BalancesModel,
    from: Account,
    to: Account,
    amount: u64,
) -> ((u64, u64), Option<TransferError>) {
    let from_balance = balances.get(&from).cloned().unwrap_or_default();
    if from_balance < amount + FEE {
        let to_balance = balances.get(&to).cloned().unwrap_or_default();
        return (
            (from_balance, to_balance),
            Some(TransferError::InsufficientFunds {
                balance: Nat::from(from_balance),
            }),
        );
    }
    balances.insert(from, from_balance - amount - FEE);

    let to_balance = balances.get(&to).cloned().unwrap_or_default();
    balances.insert(to, to_balance + amount);

    let from_balance = balances.get(&from).cloned().unwrap_or_default();
    let to_balance = balances.get(&to).cloned().unwrap_or_default();

    ((from_balance, to_balance), None)
}

fn send_transfer(
    env: &StateMachine,
    ledger: CanisterId,
    from: Principal,
    arg: &TransferArg,
) -> Result<BlockIndex, TransferError> {
    Decode!(
        &env.execute_ingress_as(
            PrincipalId(from),
            ledger,
            "icrc1_transfer",
            Encode!(arg)
            .unwrap()
        )
        .expect("failed to transfer funds")
        .bytes(),
        Result<Nat, TransferError>
    )
    .expect("failed to decode transfer response")
    .map(|n| n.0.to_u64().unwrap())
}

fn system_time_to_nanos(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64
}

fn transfer(
    env: &StateMachine,
    ledger: CanisterId,
    from: impl Into<Account>,
    to: impl Into<Account>,
    amount: u64,
) -> Result<BlockIndex, TransferError> {
    let from = from.into();
    send_transfer(
        env,
        ledger,
        from.owner,
        &TransferArg {
            from_subaccount: from.subaccount,
            to: to.into(),
            fee: None,
            created_at_time: None,
            amount: Nat::from(amount),
            memo: None,
        },
    )
}

fn list_archives(env: &StateMachine, ledger: CanisterId) -> Vec<ArchiveInfo> {
    Decode!(
        &env.query(ledger, "archives", Encode!().unwrap())
            .expect("failed to query archives")
            .bytes(),
        Vec<ArchiveInfo>
    )
    .expect("failed to decode archives response")
}

fn get_archive_transaction(env: &StateMachine, archive: Principal, block_index: u64) -> Option<Tx> {
    let canister_id =
        CanisterId::new(archive.into()).expect("failed to convert Principal to CanisterId");
    Decode!(
        &env.query(
            canister_id,
            "get_transaction",
            Encode!(&block_index).unwrap()
        )
        .expect("failed to get transaction")
        .bytes(),
        Option<Tx>
    )
    .expect("failed to decode get_transaction response")
}

fn get_transactions_as<Response: CandidType + for<'a> candid::Deserialize<'a>>(
    env: &StateMachine,
    canister: Principal,
    start: u64,
    length: usize,
    method_name: String,
) -> Response {
    let canister_id =
        CanisterId::new(canister.into()).expect("failed to convert Principal to CanisterId");
    Decode!(
        &env.query(
            canister_id,
            method_name,
            Encode!(&GetTransactionsRequest {
                start: Nat::from(start),
                length: Nat::from(length)
            })
            .unwrap()
        )
        .expect("failed to query ledger transactions")
        .bytes(),
        Response
    )
    .expect("failed to decode get_transactions response")
}

fn get_archive_transactions(
    env: &StateMachine,
    archive: Principal,
    start: u64,
    length: usize,
) -> TransactionRange {
    get_transactions_as(env, archive, start, length, "get_transactions".to_string())
}

fn get_transactions(
    env: &StateMachine,
    archive: Principal,
    start: u64,
    length: usize,
) -> GetTransactionsResponse {
    get_transactions_as(env, archive, start, length, "get_transactions".to_string())
}

fn get_blocks(
    env: &StateMachine,
    archive: Principal,
    start: u64,
    length: usize,
) -> GetBlocksResponse {
    get_transactions_as(env, archive, start, length, "get_blocks".to_string())
}

fn get_archive_blocks(
    env: &StateMachine,
    archive: Principal,
    start: u64,
    length: usize,
) -> BlockRange {
    get_transactions_as(env, archive, start, length, "get_blocks".to_string())
}

fn get_phash(block: &IcrcBlock) -> Result<Option<Hash>, String> {
    match block {
        IcrcBlock::Map(map) => {
            for (k, v) in map.iter() {
                if k == "phash" {
                    return match v {
                        IcrcBlock::Blob(blob) => blob
                            .as_slice()
                            .try_into()
                            .map(Some)
                            .map_err(|_| "phash is not a hash".to_string()),
                        _ => Err("phash should be a blob".to_string()),
                    };
                }
            }
            Ok(None)
        }
        _ => Err("top level element should be a map".to_string()),
    }
}

pub fn total_supply(env: &StateMachine, ledger: CanisterId) -> u64 {
    Decode!(
        &env.query(ledger, "icrc1_total_supply", Encode!().unwrap())
            .expect("failed to query total supply")
            .bytes(),
        Nat
    )
    .expect("failed to decode totalSupply response")
    .0
    .to_u64()
    .unwrap()
}

pub fn supported_standards(env: &StateMachine, ledger: CanisterId) -> Vec<StandardRecord> {
    Decode!(
        &env.query(ledger, "icrc1_supported_standards", Encode!().unwrap())
            .expect("failed to query supported standards")
            .bytes(),
        Vec<StandardRecord>
    )
    .expect("failed to decode icrc1_supported_standards response")
}

pub fn minting_account(env: &StateMachine, ledger: CanisterId) -> Option<Account> {
    Decode!(
        &env.query(ledger, "icrc1_minting_account", Encode!().unwrap())
            .expect("failed to query minting account icrc1")
            .bytes(),
        Option<Account>
    )
    .expect("failed to decode icrc1_minting_account response")
}

pub fn balance_of(env: &StateMachine, ledger: CanisterId, acc: impl Into<Account>) -> u64 {
    Decode!(
        &env.query(ledger, "icrc1_balance_of", Encode!(&acc.into()).unwrap())
            .expect("failed to query balance")
            .bytes(),
        Nat
    )
    .expect("failed to decode balance_of response")
    .0
    .to_u64()
    .unwrap()
}

pub fn metadata(env: &StateMachine, ledger: CanisterId) -> BTreeMap<String, Value> {
    Decode!(
        &env.query(ledger, "icrc1_metadata", Encode!().unwrap())
            .expect("failed to query metadata")
            .bytes(),
        Vec<(String, Value)>
    )
    .expect("failed to decode metadata response")
    .into_iter()
    .collect()
}

fn arb_amount() -> impl Strategy<Value = u64> {
    any::<u64>()
}

fn arb_account() -> impl Strategy<Value = Account> {
    (
        proptest::collection::vec(any::<u8>(), 28),
        any::<Option<[u8; 32]>>(),
    )
        .prop_map(|(mut principal, subaccount)| {
            principal.push(0x00);
            Account {
                owner: Principal::try_from_slice(&principal[..]).unwrap(),
                subaccount,
            }
        })
}

fn arb_transfer() -> impl Strategy<Value = Operation> {
    (
        arb_account(),
        arb_account(),
        arb_amount(),
        proptest::option::of(arb_amount()),
    )
        .prop_map(|(from, to, amount, fee)| Operation::Transfer {
            from,
            to,
            amount,
            fee,
        })
}

fn arb_mint() -> impl Strategy<Value = Operation> {
    (arb_account(), arb_amount()).prop_map(|(to, amount)| Operation::Mint { to, amount })
}

fn arb_burn() -> impl Strategy<Value = Operation> {
    (arb_account(), arb_amount()).prop_map(|(from, amount)| Operation::Burn { from, amount })
}

fn arb_operation() -> impl Strategy<Value = Operation> {
    prop_oneof![arb_transfer(), arb_mint(), arb_burn()]
}

fn arb_transaction() -> impl Strategy<Value = Transaction> {
    (
        arb_operation(),
        any::<Option<u64>>(),
        any::<Option<[u8; 32]>>(),
    )
        .prop_map(|(operation, ts, memo)| Transaction {
            operation,
            created_at_time: ts,
            memo: memo.map(|m| Memo::from(m.to_vec())),
        })
}

fn arb_block() -> impl Strategy<Value = Block> {
    (
        any::<Option<[u8; 32]>>(),
        arb_transaction(),
        proptest::option::of(any::<u64>()),
        any::<u64>(),
        proptest::option::of(arb_account()),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(parent_hash, transaction, effective_fee, ts, fee_col, fee_col_block)| Block {
                parent_hash: parent_hash.map(HashOf::new),
                transaction,
                effective_fee,
                timestamp: ts,
                fee_collector: fee_col,
                fee_collector_block_index: fee_col_block,
            },
        )
}

fn init_args(initial_balances: Vec<(Account, u64)>) -> InitArgs {
    InitArgs {
        minting_account: MINTER,
        fee_collector_account: None,
        initial_balances,
        transfer_fee: FEE,
        token_name: TOKEN_NAME.to_string(),
        token_symbol: TOKEN_SYMBOL.to_string(),
        metadata: vec![
            Value::entry(NAT_META_KEY, NAT_META_VALUE),
            Value::entry(INT_META_KEY, INT_META_VALUE),
            Value::entry(TEXT_META_KEY, TEXT_META_VALUE),
            Value::entry(BLOB_META_KEY, BLOB_META_VALUE),
        ],
        archive_options: ArchiveOptions {
            trigger_threshold: ARCHIVE_TRIGGER_THRESHOLD as usize,
            num_blocks_to_archive: NUM_BLOCKS_TO_ARCHIVE as usize,
            node_max_memory_size_bytes: None,
            max_message_size_bytes: None,
            controller_id: PrincipalId::new_user_test_id(100),
            cycles_for_archive_creation: None,
            max_transactions_per_response: None,
        },
    }
}

fn install_ledger<T>(
    env: &StateMachine,
    ledger_wasm: Vec<u8>,
    encode_init_args: fn(InitArgs) -> T,
    initial_balances: Vec<(Account, u64)>,
) -> CanisterId
where
    T: CandidType,
{
    let args = encode_init_args(init_args(initial_balances));
    let args = Encode!(&args).unwrap();
    env.install_canister(ledger_wasm, args, None).unwrap()
}

// In order to implement FI-487 in steps we need to split the test
// //rs/rosetta-api/icrc1/ledger/tests/tests.rs#test_metadata in two:
//  1. the first part that setup ledger and environemnt and tests the
//     ICRC-1 metadata that both the ICP and the ICRC-1 Ledgers have
//  2. the second part that tests the metadata that only the ICRC-1 Ledger
//     has
// Once FI-487 is done and the ICP Ledger supports all the metadata
// endpoints this function will be merged back into test_metadata here.
pub fn setup<T>(
    ledger_wasm: Vec<u8>,
    encode_init_args: fn(InitArgs) -> T,
    initial_balances: Vec<(Account, u64)>,
) -> (StateMachine, CanisterId)
where
    T: CandidType,
{
    let env = StateMachine::new();

    let canister_id = install_ledger(&env, ledger_wasm, encode_init_args, initial_balances);

    (env, canister_id)
}

pub fn test_balance_of<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let (env, canister_id) = setup(ledger_wasm.clone(), encode_init_args, vec![]);
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);

    assert_eq!(0, balance_of(&env, canister_id, p1.0));
    assert_eq!(0, balance_of(&env, canister_id, p2.0));

    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![
            (Account::from(p1.0), 10_000_000),
            (Account::from(p2.0), 5_000_000),
        ],
    );

    assert_eq!(10_000_000u64, balance_of(&env, canister_id, p1.0));
    assert_eq!(5_000_000u64, balance_of(&env, canister_id, p2.0));
}

pub fn test_metadata_icp_ledger<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    fn lookup<'a>(metadata: &'a BTreeMap<String, Value>, key: &str) -> &'a Value {
        metadata
            .get(key)
            .unwrap_or_else(|| panic!("no metadata key {} in map {:?}", key, metadata))
    }

    let (env, canister_id) = setup(ledger_wasm, encode_init_args, vec![]);

    assert_eq!(
        TOKEN_SYMBOL,
        Decode!(
            &env.query(canister_id, "icrc1_symbol", Encode!().unwrap())
                .unwrap()
                .bytes(),
            String
        )
        .unwrap()
    );

    assert_eq!(
        8,
        Decode!(
            &env.query(canister_id, "icrc1_decimals", Encode!().unwrap())
                .unwrap()
                .bytes(),
            u8
        )
        .unwrap()
    );

    let metadata = metadata(&env, canister_id);
    assert_eq!(lookup(&metadata, "icrc1:name"), &Value::from(TOKEN_NAME));
    assert_eq!(
        lookup(&metadata, "icrc1:symbol"),
        &Value::from(TOKEN_SYMBOL)
    );
    assert_eq!(lookup(&metadata, "icrc1:decimals"), &Value::from(8u64));

    let standards = supported_standards(&env, canister_id);
    assert_eq!(
        standards,
        vec![StandardRecord {
            name: "ICRC-1".to_string(),
            url: "https://github.com/dfinity/ICRC-1".to_string(),
        }]
    );
}
pub fn test_metadata<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    fn lookup<'a>(metadata: &'a BTreeMap<String, Value>, key: &str) -> &'a Value {
        metadata
            .get(key)
            .unwrap_or_else(|| panic!("no metadata key {} in map {:?}", key, metadata))
    }

    let (env, canister_id) = setup(ledger_wasm, encode_init_args, vec![]);

    assert_eq!(
        TOKEN_SYMBOL,
        Decode!(
            &env.query(canister_id, "icrc1_symbol", Encode!().unwrap())
                .unwrap()
                .bytes(),
            String
        )
        .unwrap()
    );

    assert_eq!(
        8,
        Decode!(
            &env.query(canister_id, "icrc1_decimals", Encode!().unwrap())
                .unwrap()
                .bytes(),
            u8
        )
        .unwrap()
    );

    let metadata = metadata(&env, canister_id);
    assert_eq!(lookup(&metadata, "icrc1:name"), &Value::from(TOKEN_NAME));
    assert_eq!(
        lookup(&metadata, "icrc1:symbol"),
        &Value::from(TOKEN_SYMBOL)
    );
    assert_eq!(lookup(&metadata, "icrc1:decimals"), &Value::from(8u64));
    //Not all ICRC-1 impelmentations have the same metadata entries. Thus only certain basic fields are shared by all ICRC-1 implementaions
    assert_eq!(
        lookup(&metadata, NAT_META_KEY),
        &Value::from(NAT_META_VALUE)
    );
    assert_eq!(
        lookup(&metadata, INT_META_KEY),
        &Value::from(INT_META_VALUE)
    );
    assert_eq!(
        lookup(&metadata, TEXT_META_KEY),
        &Value::from(TEXT_META_VALUE)
    );
    assert_eq!(
        lookup(&metadata, BLOB_META_KEY),
        &Value::from(BLOB_META_VALUE)
    );
    let standards = supported_standards(&env, canister_id);
    assert_eq!(
        standards,
        vec![StandardRecord {
            name: "ICRC-1".to_string(),
            url: "https://github.com/dfinity/ICRC-1".to_string(),
        }]
    );
}

pub fn test_total_supply<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);
    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![
            (Account::from(p1.0), 10_000_000),
            (Account::from(p2.0), 5_000_000),
        ],
    );
    assert_eq!(15_000_000, total_supply(&env, canister_id));
}

pub fn test_minting_account<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let (env, canister_id) = setup(ledger_wasm, encode_init_args, vec![]);
    assert_eq!(Some(MINTER), minting_account(&env, canister_id));
}

pub fn test_single_transfer<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);
    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![
            (Account::from(p1.0), 10_000_000),
            (Account::from(p2.0), 5_000_000),
        ],
    );

    assert_eq!(15_000_000, total_supply(&env, canister_id));
    assert_eq!(10_000_000u64, balance_of(&env, canister_id, p1.0));
    assert_eq!(5_000_000u64, balance_of(&env, canister_id, p2.0));

    transfer(&env, canister_id, p1.0, p2.0, 1_000_000).expect("transfer failed");

    assert_eq!(15_000_000 - FEE, total_supply(&env, canister_id));
    assert_eq!(9_000_000u64 - FEE, balance_of(&env, canister_id, p1.0));
    assert_eq!(6_000_000u64, balance_of(&env, canister_id, p2.0));
}

pub fn test_tx_deduplication<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);
    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![(Account::from(p1.0), 10_000_000)],
    );
    // No created_at_time => no deduplication
    let block_id = transfer(&env, canister_id, p1.0, p2.0, 10_000).expect("transfer failed");
    assert!(transfer(&env, canister_id, p1.0, p2.0, 10_000).expect("transfer failed") > block_id);

    let now = system_time_to_nanos(env.time());

    let transfer_args = TransferArg {
        from_subaccount: None,
        to: p2.0.into(),
        fee: None,
        amount: Nat::from(1_000_000),
        created_at_time: Some(now),
        memo: None,
    };

    let block_idx =
        send_transfer(&env, canister_id, p1.0, &transfer_args).expect("transfer failed");

    assert_eq!(
        send_transfer(&env, canister_id, p1.0, &transfer_args),
        Err(TransferError::Duplicate {
            duplicate_of: Nat::from(block_idx)
        })
    );

    // Same transaction, but with the fee set explicitly.
    // The Ledger should not deduplicate.
    let args = TransferArg {
        fee: Some(Nat::from(10_000)),
        ..transfer_args.clone()
    };
    let block_idx = send_transfer(&env, canister_id, p1.0, &args)
        .expect("transfer should not be deduplicated because the fee was set explicitly this time");

    // This time the transaction is a duplicate.
    assert_eq!(
        Err(TransferError::Duplicate {
            duplicate_of: Nat::from(block_idx)
        }),
        send_transfer(&env, canister_id, p1.0, &args,)
    );

    env.advance_time(TX_WINDOW + Duration::from_secs(5 * 60));
    let now = system_time_to_nanos(env.time());

    assert_eq!(
        send_transfer(&env, canister_id, p1.0, &transfer_args,),
        Err(TransferError::TooOld),
    );

    // Same transaction, but `created_at_time` specified explicitly.
    // The ledger should not deduplicate this request.
    let block_idx = send_transfer(
        &env,
        canister_id,
        p1.0,
        &TransferArg {
            from_subaccount: None,
            to: p2.0.into(),
            fee: None,
            amount: Nat::from(1_000_000),
            created_at_time: Some(now),
            memo: None,
        },
    )
    .expect("transfer failed");

    // This time the transaction is a duplicate.
    assert_eq!(
        Err(TransferError::Duplicate {
            duplicate_of: Nat::from(block_idx)
        }),
        send_transfer(
            &env,
            canister_id,
            p1.0,
            &TransferArg {
                from_subaccount: None,
                to: p2.0.into(),
                fee: None,
                amount: Nat::from(1_000_000),
                created_at_time: Some(now),
                memo: None,
            }
        )
    );

    // Same transaction, but with "default" `memo`.
    // The ledger should not deduplicate because we set a new field explicitly.
    let block_idx = send_transfer(
        &env,
        canister_id,
        p1.0,
        &TransferArg {
            from_subaccount: None,
            to: p2.0.into(),
            fee: None,
            amount: Nat::from(1_000_000),
            created_at_time: Some(now),
            memo: Some(Memo::default()),
        },
    )
    .expect("transfer failed");

    // This time the transaction is a duplicate.
    assert_eq!(
        Err(TransferError::Duplicate {
            duplicate_of: Nat::from(block_idx)
        }),
        send_transfer(
            &env,
            canister_id,
            p1.0,
            &TransferArg {
                from_subaccount: None,
                to: p2.0.into(),
                fee: None,
                amount: Nat::from(1_000_000),
                created_at_time: Some(now),
                memo: Some(Memo::default()),
            }
        )
    );
}

pub fn test_mint_burn<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let (env, canister_id) = setup(ledger_wasm, encode_init_args, vec![]);
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);

    assert_eq!(0, total_supply(&env, canister_id));
    assert_eq!(0, balance_of(&env, canister_id, p1.0));
    assert_eq!(0, balance_of(&env, canister_id, MINTER));

    transfer(&env, canister_id, MINTER, p1.0, 10_000_000).expect("mint failed");

    assert_eq!(10_000_000, total_supply(&env, canister_id));
    assert_eq!(10_000_000, balance_of(&env, canister_id, p1.0));
    assert_eq!(0, balance_of(&env, canister_id, MINTER));

    transfer(&env, canister_id, p1.0, MINTER, 1_000_000).expect("burn failed");

    assert_eq!(9_000_000, total_supply(&env, canister_id));
    assert_eq!(9_000_000, balance_of(&env, canister_id, p1.0));
    assert_eq!(0, balance_of(&env, canister_id, MINTER));

    // You have at least FEE, you can burn at least FEE
    assert_eq!(
        Err(TransferError::BadBurn {
            min_burn_amount: Nat::from(FEE)
        }),
        transfer(&env, canister_id, p1.0, MINTER, FEE / 2),
    );

    transfer(&env, canister_id, p1.0, p2.0, FEE / 2).expect("transfer failed");

    assert_eq!(FEE / 2, balance_of(&env, canister_id, p2.0));

    // If you have less than FEE, you can burn only the whole amount.
    assert_eq!(
        Err(TransferError::BadBurn {
            min_burn_amount: Nat::from(FEE / 2)
        }),
        transfer(&env, canister_id, p2.0, MINTER, FEE / 4),
    );
    transfer(&env, canister_id, p2.0, MINTER, FEE / 2).expect("burn failed");

    assert_eq!(0, balance_of(&env, canister_id, p2.0));

    // You cannot burn zero tokens, no matter what your balance is.
    assert_eq!(
        Err(TransferError::BadBurn {
            min_burn_amount: Nat::from(FEE)
        }),
        transfer(&env, canister_id, p2.0, MINTER, 0),
    );
}

pub fn test_account_canonicalization<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);
    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![
            (Account::from(p1.0), 10_000_000),
            (Account::from(p2.0), 5_000_000),
        ],
    );

    assert_eq!(
        10_000_000u64,
        balance_of(
            &env,
            canister_id,
            Account {
                owner: p1.0,
                subaccount: None
            }
        )
    );
    assert_eq!(
        10_000_000u64,
        balance_of(
            &env,
            canister_id,
            Account {
                owner: p1.0,
                subaccount: Some([0; 32])
            }
        )
    );

    transfer(
        &env,
        canister_id,
        p1.0,
        Account {
            owner: p2.0,
            subaccount: Some([0; 32]),
        },
        1_000_000,
    )
    .expect("transfer failed");

    assert_eq!(
        6_000_000u64,
        balance_of(
            &env,
            canister_id,
            Account {
                owner: p2.0,
                subaccount: None
            }
        )
    );
}

pub fn test_tx_time_bounds<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);
    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![(Account::from(p1.0), 10_000_000)],
    );

    let now = system_time_to_nanos(env.time());
    let tx_window = TX_WINDOW.as_nanos() as u64;

    assert_eq!(
        Err(TransferError::TooOld),
        send_transfer(
            &env,
            canister_id,
            p1.0,
            &TransferArg {
                from_subaccount: None,
                to: p2.0.into(),
                fee: None,
                amount: Nat::from(1_000_000),
                created_at_time: Some(now - tx_window - 1),
                memo: None,
            }
        )
    );

    assert_eq!(
        Err(TransferError::CreatedInFuture { ledger_time: now }),
        send_transfer(
            &env,
            canister_id,
            p1.0,
            &TransferArg {
                from_subaccount: None,
                to: p2.0.into(),
                fee: None,
                amount: Nat::from(1_000_000),
                created_at_time: Some(now + Duration::from_secs(5 * 60).as_nanos() as u64),
                memo: None
            }
        )
    );

    assert_eq!(10_000_000u64, balance_of(&env, canister_id, p1.0));
    assert_eq!(0u64, balance_of(&env, canister_id, p2.0));
}

pub fn test_archiving<T>(
    ledger_wasm: Vec<u8>,
    encode_init_args: fn(InitArgs) -> T,
    archive_wasm: Vec<u8>,
) where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);

    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![(Account::from(p1.0), 10_000_000)],
    );

    for i in 0..ARCHIVE_TRIGGER_THRESHOLD {
        transfer(&env, canister_id, p1.0, p2.0, 10_000 + i).expect("transfer failed");
    }

    env.run_until_completion(/*max_ticks=*/ 10);

    let archive_info = list_archives(&env, canister_id);
    assert_eq!(archive_info.len(), 1);
    assert_eq!(archive_info[0].block_range_start, 0);
    assert_eq!(archive_info[0].block_range_end, NUM_BLOCKS_TO_ARCHIVE - 1);

    let archive_principal = archive_info[0].canister_id;

    let resp = get_transactions(&env, canister_id.get().0, 0, 1_000_000);
    assert_eq!(resp.first_index, Nat::from(NUM_BLOCKS_TO_ARCHIVE));
    assert_eq!(
        resp.transactions.len(),
        (ARCHIVE_TRIGGER_THRESHOLD - NUM_BLOCKS_TO_ARCHIVE + 1) as usize
    );
    assert_eq!(resp.archived_transactions.len(), 1);
    assert_eq!(resp.archived_transactions[0].start, Nat::from(0));
    assert_eq!(
        resp.archived_transactions[0].length,
        Nat::from(NUM_BLOCKS_TO_ARCHIVE)
    );

    let archived_transactions =
        get_archive_transactions(&env, archive_principal, 0, NUM_BLOCKS_TO_ARCHIVE as usize)
            .transactions;

    for i in 1..NUM_BLOCKS_TO_ARCHIVE {
        let expected_tx = Transfer {
            from: Account {
                owner: p1.0,
                subaccount: None,
            },
            to: Account {
                owner: p2.0,
                subaccount: None,
            },
            amount: Nat::from(10_000 + i - 1),
            fee: Some(Nat::from(FEE)),
            memo: None,
            created_at_time: None,
        };
        let tx = get_archive_transaction(&env, archive_principal, i).unwrap();
        assert_eq!(tx.transfer.as_ref(), Some(&expected_tx));
        let tx = archived_transactions[i as usize].clone();
        assert_eq!(tx.transfer.as_ref(), Some(&expected_tx));
    }

    // Check that requesting non-existing blocks does not crash the ledger.
    let missing_blocks_reply = get_transactions(&env, canister_id.get().0, 100, 5);
    assert_eq!(0, missing_blocks_reply.transactions.len());
    assert_eq!(0, missing_blocks_reply.archived_transactions.len());

    // Upgrade the archive and check that the data is still available.

    let archive_canister_id = CanisterId::new(archive_principal.into())
        .expect("failed to convert Principal to CanisterId");

    env.upgrade_canister(archive_canister_id, archive_wasm, vec![])
        .expect("failed to upgrade the archive canister");

    for i in 1..NUM_BLOCKS_TO_ARCHIVE {
        let tx = get_archive_transaction(&env, archive_principal, i).unwrap();
        assert_eq!(
            tx.transfer,
            Some(Transfer {
                from: Account {
                    owner: p1.0,
                    subaccount: None
                },
                to: Account {
                    owner: p2.0,
                    subaccount: None
                },
                amount: Nat::from(10_000 + i - 1),
                fee: Some(Nat::from(FEE)),
                memo: None,
                created_at_time: None,
            })
        );
    }

    // Check that we can append more blocks after the upgrade.
    for i in 0..(ARCHIVE_TRIGGER_THRESHOLD - NUM_BLOCKS_TO_ARCHIVE) {
        transfer(&env, canister_id, p1.0, p2.0, 20_000 + i).expect("transfer failed");
    }

    let archive_info = list_archives(&env, canister_id);
    assert_eq!(archive_info.len(), 1);
    assert_eq!(archive_info[0].block_range_start, 0);
    assert_eq!(
        archive_info[0].block_range_end,
        2 * NUM_BLOCKS_TO_ARCHIVE - 1
    );

    // Check that the archive handles requested ranges correctly.
    let archived_transactions =
        get_archive_transactions(&env, archive_principal, 0, 1_000_000).transactions;
    let n = 2 * NUM_BLOCKS_TO_ARCHIVE as usize;
    assert_eq!(archived_transactions.len(), n);

    for start in 0..n {
        for end in start..n {
            let tx = get_archive_transactions(&env, archive_principal, start as u64, end - start)
                .transactions;
            assert_eq!(archived_transactions[start..end], tx);
        }
    }
}

pub fn test_get_blocks<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let p1 = PrincipalId::new_user_test_id(1);
    let p2 = PrincipalId::new_user_test_id(2);

    let (env, canister_id) = setup(
        ledger_wasm,
        encode_init_args,
        vec![(Account::from(p1.0), 10_000_000)],
    );

    for i in 0..ARCHIVE_TRIGGER_THRESHOLD {
        transfer(&env, canister_id, p1.0, p2.0, 10_000 + i * 10_000).expect("transfer failed");
    }

    env.run_until_completion(/*max_ticks=*/ 10);

    let resp = get_blocks(&env, canister_id.get().0, 0, 1_000_000);
    assert_eq!(resp.first_index, Nat::from(NUM_BLOCKS_TO_ARCHIVE));
    assert_eq!(
        resp.blocks.len(),
        (ARCHIVE_TRIGGER_THRESHOLD - NUM_BLOCKS_TO_ARCHIVE + 1) as usize
    );
    assert_eq!(resp.archived_blocks.len(), 1);
    assert_eq!(resp.archived_blocks[0].start, Nat::from(0));
    assert_eq!(
        resp.archived_blocks[0].length,
        Nat::from(NUM_BLOCKS_TO_ARCHIVE)
    );
    assert!(resp.certificate.is_some());

    let archive_canister_id = list_archives(&env, canister_id)[0].canister_id;
    let archived_blocks =
        get_archive_blocks(&env, archive_canister_id, 0, NUM_BLOCKS_TO_ARCHIVE as usize).blocks;
    assert_eq!(archived_blocks.len(), NUM_BLOCKS_TO_ARCHIVE as usize);

    let mut prev_hash = None;

    // Check that the hash chain is correct.
    for block in archived_blocks.into_iter().chain(resp.blocks.into_iter()) {
        assert_eq!(
            prev_hash,
            get_phash(&block).expect("cannot get the hash of the previous block")
        );
        prev_hash = Some(block.hash());
    }

    // Check that requesting non-existing blocks does not crash the ledger.
    let missing_blocks_reply = get_blocks(&env, canister_id.get().0, 100, 5);
    assert_eq!(0, missing_blocks_reply.blocks.len());
    assert_eq!(0, missing_blocks_reply.archived_blocks.len());
}

// Generate random blocks and check that their CBOR encoding complies with the CDDL spec.
pub fn block_encoding_agrees_with_the_schema() {
    use std::path::PathBuf;

    let block_cddl_path =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("block.cddl");
    let block_cddl =
        String::from_utf8(std::fs::read(block_cddl_path).expect("failed to read block.cddl file"))
            .unwrap();

    let mut runner = TestRunner::default();
    runner
        .run(&arb_block(), |block| {
            let cbor_bytes = block.encode().into_vec();
            cddl::validate_cbor_from_slice(&block_cddl, &cbor_bytes, None).map_err(|e| {
                TestCaseError::fail(format!(
                    "Failed to validate CBOR: {} (inspect it on https://cbor.me), error: {}",
                    hex::encode(&cbor_bytes),
                    e
                ))
            })
        })
        .unwrap();
}

// Check that different blocks produce different hashes.
pub fn transaction_hashes_are_unique() {
    let mut runner = TestRunner::default();
    runner
        .run(&(arb_transaction(), arb_transaction()), |(lhs, rhs)| {
            use ic_ledger_canister_core::ledger::LedgerTransaction;

            prop_assume!(lhs != rhs);
            prop_assert_ne!(lhs.hash(), rhs.hash());

            Ok(())
        })
        .unwrap();
}

pub fn block_hashes_are_unique() {
    let mut runner = TestRunner::default();
    runner
        .run(&(arb_block(), arb_block()), |(lhs, rhs)| {
            prop_assume!(lhs != rhs);

            let lhs_hash = Block::block_hash(&lhs.encode());
            let rhs_hash = Block::block_hash(&rhs.encode());

            prop_assert_ne!(lhs_hash, rhs_hash);
            Ok(())
        })
        .unwrap();
}

// Generate random blocks and check that the block hash is stable.
pub fn block_hashes_are_stable() {
    let mut runner = TestRunner::default();
    runner
        .run(&arb_block(), |block| {
            let encoded_block = block.encode();
            let hash1 = Block::block_hash(&encoded_block);
            let decoded = Block::decode(encoded_block).unwrap();
            let hash2 = Block::block_hash(&decoded.encode());
            prop_assert_eq!(hash1, hash2);
            Ok(())
        })
        .unwrap();
}

pub fn check_transfer_model<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    use proptest::collection::vec as pvec;

    const NUM_ACCOUNTS: usize = 10;
    const MIN_TRANSACTIONS: usize = 5;
    const MAX_TRANSACTIONS: usize = 10;
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(5));
    runner
        .run(
            &(
                pvec(arb_account(), NUM_ACCOUNTS),
                pvec(0..10_000_000u64, NUM_ACCOUNTS),
                pvec(
                    (0..NUM_ACCOUNTS, 0..NUM_ACCOUNTS, 0..1_000_000_000u64),
                    MIN_TRANSACTIONS..MAX_TRANSACTIONS,
                ),
            ),
            |(accounts, mints, transfers)| {
                test_transfer_model(
                    accounts,
                    mints,
                    transfers,
                    ledger_wasm.clone(),
                    encode_init_args,
                )
            },
        )
        .unwrap();
}

pub fn test_upgrade<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let (env, canister_id) = setup(ledger_wasm.clone(), encode_init_args, vec![]);

    let metadata_res = metadata(&env, canister_id);
    let metadata_value = metadata_res.get(TEXT_META_KEY).unwrap();
    assert_eq!(*metadata_value, Value::Text(TEXT_META_VALUE.to_string()));

    const OTHER_TOKEN_SYMBOL: &str = "NEWSYMBOL";
    const OTHER_TOKEN_NAME: &str = "NEWTKNNAME";
    const NEW_FEE: u64 = 1234;

    let upgrade_args = LedgerArgument::Upgrade(Some(UpgradeArgs {
        metadata: Some(vec![(
            TEXT_META_KEY.into(),
            Value::Text(TEXT_META_VALUE_2.into()),
        )]),
        token_name: Some(OTHER_TOKEN_NAME.into()),
        token_symbol: Some(OTHER_TOKEN_SYMBOL.into()),
        transfer_fee: Some(NEW_FEE),
        ..UpgradeArgs::default()
    }));

    env.upgrade_canister(canister_id, ledger_wasm, Encode!(&upgrade_args).unwrap())
        .expect("failed to upgrade the archive canister");

    let metadata_res_after_upgrade = metadata(&env, canister_id);
    assert_eq!(
        *metadata_res_after_upgrade.get(TEXT_META_KEY).unwrap(),
        Value::Text(TEXT_META_VALUE_2.to_string())
    );

    let token_symbol_after_upgrade: String = Decode!(
        &env.query(canister_id, "icrc1_symbol", Encode!().unwrap())
            .expect("failed to query symbol")
            .bytes(),
        String
    )
    .expect("failed to decode balance_of response");
    assert_eq!(token_symbol_after_upgrade, OTHER_TOKEN_SYMBOL);

    let token_name_after_upgrade: String = Decode!(
        &env.query(canister_id, "icrc1_name", Encode!().unwrap())
            .expect("failed to query name")
            .bytes(),
        String
    )
    .expect("failed to decode balance_of response");
    assert_eq!(token_name_after_upgrade, OTHER_TOKEN_NAME);

    let token_fee_after_upgrade: u64 = Decode!(
        &env.query(canister_id, "icrc1_fee", Encode!().unwrap())
            .expect("failed to query fee")
            .bytes(),
        Nat
    )
    .expect("failed to decode balance_of response")
    .0
    .to_u64()
    .unwrap();
    assert_eq!(token_fee_after_upgrade, NEW_FEE);
}

pub fn test_fee_collector<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let env = StateMachine::new();
    // by default the fee collector is not set
    let ledger_id = install_ledger(&env, ledger_wasm.clone(), encode_init_args, vec![]);
    // only 1 test case because we modify the ledger within the test
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(1));
    runner
        .run(
            &(
                arb_account(),
                arb_account(),
                arb_account(),
                1..10_000_000u64,
            )
                .prop_filter("The three accounts must be different", |(a1, a2, a3, _)| {
                    a1 != a2 && a2 != a3 && a1 != a3
                }),
            |(account_from, account_to, fee_collector, amount)| {
                // test 1: with no fee collector the fee should be burned

                // mint some tokens for a user
                transfer(&env, ledger_id, MINTER, account_from, 3 * (amount + FEE))
                    .expect("Unable to mint tokens");

                // record the previous total_supply and make the transfer.
                let total_supply_before = total_supply(&env, ledger_id);
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform transfer");

                // if the fee was burned then the total_supply after the
                // transfer should be the one before plus the (burned) FEE
                assert_eq!(
                    total_supply_before,
                    total_supply(&env, ledger_id) + FEE,
                    "Total supply should have been decreased of the (burned) fee {}",
                    FEE
                );

                // test 2: upgrade the ledger to have a fee collector.
                //         The fee should be collected by the fee collector

                // set the fee collector
                let ledger_upgrade_arg = LedgerArgument::Upgrade(Some(UpgradeArgs {
                    change_fee_collector: Some(ChangeFeeCollector::SetTo(fee_collector)),
                    ..UpgradeArgs::default()
                }));
                env.upgrade_canister(
                    ledger_id,
                    ledger_wasm.clone(),
                    Encode!(&ledger_upgrade_arg).unwrap(),
                )
                .unwrap();

                // record the previous total_supply and make the transfer.
                let total_supply_before = total_supply(&env, ledger_id);
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform transfer");

                // if the fee was burned then the total_supply after the
                // transfer should be the one before (nothing burned)
                assert_eq!(
                    total_supply_before,
                    total_supply(&env, ledger_id),
                    "Total supply shouldn't have changed"
                );

                // the fee collector must have collected the fee
                assert_eq!(
                    FEE,
                    balance_of(&env, ledger_id, fee_collector),
                    "The fee_collector should have collected the fee"
                );

                // test 3: upgrade the ledger to not have a fee collector.
                //         The fee should once again be burned.

                // unset the fee collector
                let ledger_upgrade_arg = LedgerArgument::Upgrade(Some(UpgradeArgs {
                    change_fee_collector: Some(ChangeFeeCollector::Unset),
                    ..UpgradeArgs::default()
                }));
                env.upgrade_canister(
                    ledger_id,
                    ledger_wasm.clone(),
                    Encode!(&ledger_upgrade_arg).unwrap(),
                )
                .unwrap();

                // record the previous total_supply and make the transfer.
                let total_supply_before = total_supply(&env, ledger_id);
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform transfer");

                // if the fee was burned then the total_supply after the
                // transfer should be the one before plus the (burned) FEE
                assert_eq!(
                    total_supply_before,
                    total_supply(&env, ledger_id) + FEE,
                    "Total supply should have been decreased of the (burned) fee {}",
                    FEE
                );

                // the fee collector must have collected no fee this time
                assert_eq!(
                    FEE,
                    balance_of(&env, ledger_id, fee_collector),
                    "The fee_collector should have collected the fee"
                );

                Ok(())
            },
        )
        .unwrap();
}

pub fn test_fee_collector_blocks<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    fn value_as_u64(value: icrc_ledger_types::icrc::generic_value::Value) -> u64 {
        match value {
            icrc_ledger_types::icrc::generic_value::Value::Nat(n) => {
                n.0.to_u64().expect("Bloc index must be a u64")
            }
            value => panic!("Expected Value::Nat but found {:?}", value),
        }
    }

    fn value_as_account(value: icrc_ledger_types::icrc::generic_value::Value) -> Account {
        use icrc_ledger_types::icrc::generic_value::Value;

        match value {
            Value::Array(array) => match &array[..] {
                [Value::Blob(principal_bytes)] => Account {
                    owner: Principal::try_from(principal_bytes.as_ref())
                        .expect("failed to parse account owner"),
                    subaccount: None,
                },
                [Value::Blob(principal_bytes), Value::Blob(subaccount_bytes)] => Account {
                    owner: Principal::try_from(principal_bytes.as_ref())
                        .expect("failed to parse account owner"),
                    subaccount: Some(
                        Subaccount::try_from(subaccount_bytes.as_ref())
                            .expect("failed to parse subaccount"),
                    ),
                },
                _ => panic!("Unexpected account representation: {:?}", array),
            },
            value => panic!("Expected Value::Array but found {:?}", value),
        }
    }

    fn fee_collector_from_block(
        block: icrc_ledger_types::icrc::generic_value::Value,
    ) -> (Option<Account>, Option<u64>) {
        match block {
            icrc_ledger_types::icrc::generic_value::Value::Map(block_map) => {
                let fee_collector = block_map
                    .get("fee_col")
                    .map(|fee_collector| value_as_account(fee_collector.clone()));
                let fee_collector_block_index = block_map
                    .get("fee_col_block")
                    .map(|value| value_as_u64(value.clone()));
                (fee_collector, fee_collector_block_index)
            }
            _ => panic!("A block should be a map!"),
        }
    }

    let env = StateMachine::new();
    // only 1 test case because we modify the ledger within the test
    let mut runner = TestRunner::new(TestRunnerConfig::with_cases(1));
    runner
        .run(
            &(
                arb_account(),
                arb_account(),
                arb_account(),
                1..10_000_000u64,
            )
                .prop_filter("The three accounts must be different", |(a1, a2, a3, _)| {
                    a1 != a2 && a2 != a3 && a1 != a3
                }),
            |(account_from, account_to, fee_collector_account, amount)| {
                let args = encode_init_args(InitArgs {
                    fee_collector_account: Some(fee_collector_account),
                    initial_balances: vec![(account_from, (amount + FEE) * 6)],
                    ..init_args(vec![])
                });
                let args = Encode!(&args).unwrap();
                let ledger_id = env
                    .install_canister(ledger_wasm.clone(), args, None)
                    .unwrap();

                // The block at index 0 is the minting operation for account_from and
                // has the fee collector set.
                // Make 2 more transfers that should point to the first block index
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform the transfer");
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform the transfer");

                let blocks = get_blocks(&env, ledger_id.get().0, 0, 4).blocks;

                // the first block must have the fee collector explicitly defined
                assert_eq!(
                    fee_collector_from_block(blocks.get(0).unwrap().clone()),
                    (Some(fee_collector_account), None)
                );
                // the other two blocks must have a pointer to the first block
                assert_eq!(
                    fee_collector_from_block(blocks.get(1).unwrap().clone()),
                    (None, Some(0))
                );
                assert_eq!(
                    fee_collector_from_block(blocks.get(2).unwrap().clone()),
                    (None, Some(0))
                );

                // change the fee collector to a new one. The next block must have
                // the fee collector set while the ones that follow will point
                // to that one
                let ledger_upgrade_arg = LedgerArgument::Upgrade(Some(UpgradeArgs {
                    change_fee_collector: Some(ChangeFeeCollector::SetTo(account_from)),
                    ..UpgradeArgs::default()
                }));
                env.upgrade_canister(
                    ledger_id,
                    ledger_wasm.clone(),
                    Encode!(&ledger_upgrade_arg).unwrap(),
                )
                .unwrap();

                let block_id = transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform the transfer");
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform the transfer");
                transfer(&env, ledger_id, account_from, account_to, amount)
                    .expect("Unable to perform the transfer");
                let blocks = get_blocks(&env, ledger_id.get().0, block_id, 3).blocks;
                assert_eq!(
                    fee_collector_from_block(blocks.get(0).unwrap().clone()),
                    (Some(account_from), None)
                );
                assert_eq!(
                    fee_collector_from_block(blocks.get(1).unwrap().clone()),
                    (None, Some(block_id))
                );
                assert_eq!(
                    fee_collector_from_block(blocks.get(2).unwrap().clone()),
                    (None, Some(block_id))
                );

                Ok(())
            },
        )
        .unwrap()
}

pub fn test_memo_max_len<T>(ledger_wasm: Vec<u8>, encode_init_args: fn(InitArgs) -> T)
where
    T: CandidType,
{
    let from_account = Principal::from_slice(&[1u8; 29]).into();
    let (env, ledger_id) = setup(
        ledger_wasm.clone(),
        encode_init_args,
        vec![(from_account, 1_000_000_000)],
    );
    let to_account = Principal::from_slice(&[2u8; 29]).into();
    let transfer_with_memo = |memo: &[u8]| -> Result<WasmResult, UserError> {
        env.execute_ingress_as(
            PrincipalId(from_account.owner),
            ledger_id,
            "icrc1_transfer",
            Encode!(&TransferArg {
                from_subaccount: None,
                to: to_account,
                amount: Nat::from(1),
                fee: None,
                created_at_time: None,
                memo: Some(Memo::from(memo.to_vec())),
            })
            .unwrap(),
        )
    };

    // We didn't set the max_memo_length in the init params of the ledger
    // so the memo will be accepted only if it's 32 bytes or less
    for i in 0..=32 {
        assert!(
            transfer_with_memo(&vec![0u8; i]).is_ok(),
            "Memo size: {}",
            i
        );
    }
    expect_memo_length_error(transfer_with_memo, &[0u8; 33]);

    // Change the memo to 64 bytes
    let args = ic_icrc1_ledger::LedgerArgument::Upgrade(Some(ic_icrc1_ledger::UpgradeArgs {
        max_memo_length: Some(64),
        ..ic_icrc1_ledger::UpgradeArgs::default()
    }));
    let args = Encode!(&args).unwrap();
    env.upgrade_canister(ledger_id, ledger_wasm.clone(), args)
        .unwrap();

    // now the ledger should accept memos up to 64 bytes
    for i in 0..=64 {
        assert!(
            transfer_with_memo(&vec![0u8; i]).is_ok(),
            "Memo size: {}",
            i
        );
    }
    expect_memo_length_error(transfer_with_memo, &[0u8; 65]);

    expect_memo_length_error(transfer_with_memo, &[0u8; u16::MAX as usize + 1]);

    // Trying to shrink the memo should result in a failure
    let args = ic_icrc1_ledger::LedgerArgument::Upgrade(Some(ic_icrc1_ledger::UpgradeArgs {
        max_memo_length: Some(63),
        ..ic_icrc1_ledger::UpgradeArgs::default()
    }));
    let args = Encode!(&args).unwrap();
    assert!(env.upgrade_canister(ledger_id, ledger_wasm, args).is_err());
}

fn expect_memo_length_error<T>(transfer_with_memo: T, memo: &[u8])
where
    T: FnOnce(&[u8]) -> Result<WasmResult, UserError>,
{
    match transfer_with_memo(memo) {
        Err(user_error) => assert_eq!(
            user_error.code(),
            ErrorCode::CanisterCalledTrap,
            "unexpected error: {}",
            user_error
        ),
        Ok(result) => panic!(
            "expected a reject for a {}-byte memo, got result {:?}",
            memo.len(),
            result
        ),
    }
}
