use alloy::primitives::{Address, B256, Bytes, U256};
use jsonrpsee::{
    core::RpcResult,
    types::{ErrorObjectOwned, Params, error::ErrorCode},
};
use primitives::{
    constants::{CHAIN_ID, GAS_PRICE},
    node::{BlockState, LogFilter},
    params::{BlockTag, CallRequest, LogFilterRequest},
    response::{
        BlockResponse, TransactionReceiptResponse, TransactionResponse, data_bytes, data_hash,
        log_json, quantity,
    },
};
use serde_json::Value;

use node::{
    evm::BreejaDb,
    executor::{
        BlockContext, CALL_GAS_CEILING, CallResult, CallSpec, EstimateError, estimate_gas,
        simulate_call,
    },
};

use crate::RPCContext;

/// JSON-RPC error code Ethereum clients use for "your request was understood but
/// is not valid", as opposed to a malformed one.
const INVALID_INPUT: i32 = -32000;

/// Turns an internal error into a JSON-RPC error object the client can read.
fn invalid_input(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(INVALID_INPUT, error.to_string(), None::<()>)
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        ErrorCode::InvalidParams.code(),
        error.to_string(),
        None::<()>,
    )
}

/// Parses a 32-byte hash parameter.
fn parse_hash(raw: &str) -> Result<B256, ErrorObjectOwned> {
    raw.parse::<B256>()
        .map_err(|e| invalid_params(format!("invalid 32-byte hash {raw:?}: {e}")))
}

/// Resolves a block tag against the chain head, erroring on a block that does not
/// exist rather than silently substituting the latest one.
fn resolve_tag(state: &BlockState, tag: &BlockTag) -> Result<u64, ErrorObjectOwned> {
    tag.resolve(state.get_block_number()).map_err(invalid_input)
}


/// The chain's id, used by wallets for EIP-155 replay protection.
pub fn chain_id_handler() -> String {
    quantity(CHAIN_ID)
}


/// The number of the chain head. A freshly started node sits at `0x0`, the genesis
/// block, and climbs from there as the miner seals blocks.
pub fn block_number_handler(context: &RPCContext) -> String {
    quantity(context.chain.read().get_block_number())
}


/// Renders a block, inlining full transaction objects if the caller asked for them.
fn render_block(state: &BlockState, number: u64, full_transactions: bool) -> Value {
    let Some(block) = state.get_block(number) else {
        return Value::Null;
    };

    let transactions: Vec<Value> = if full_transactions {
        block
            .transactions
            .iter()
            .filter_map(|hash| state.get_transaction(hash))
            .map(|tx| {
                serde_json::to_value(TransactionResponse::from(tx))
                    .expect("a transaction response always serializes")
            })
            .collect()
    } else {
        block
            .transactions
            .iter()
            .map(|hash| Value::String(data_hash(hash)))
            .collect()
    };

    serde_json::to_value(BlockResponse::from_block(block, transactions))
        .expect("a block response always serializes")
}

pub fn get_block_by_number_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    // Ethereum's optional trailing parameters have to be read one at a time: a
    // client may send just the block, or the block and the `fullTransactions`
    // flag, and both are valid.
    let mut sequence = params.sequence();
    let tag: BlockTag = sequence.next()?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    Ok(render_block(&state, number, full_transactions))
}

pub fn get_block_by_hash_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Value> {
    let mut sequence = params.sequence();
    let hash = parse_hash(&sequence.next::<String>()?)?;
    let full_transactions = sequence.optional_next::<bool>()?.unwrap_or(false);

    let state = context.chain.read();
    // An unknown hash is `null`, not an error: the caller asked a legitimate
    // question and the answer is "no such block".
    let Some(number) = state.get_block_by_hash(&hash).map(|block| block.number) else {
        return Ok(Value::Null);
    };
    Ok(render_block(&state, number, full_transactions))
}



pub fn get_transaction_by_hash_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_transaction(&hash) {
        Some(tx) => Ok(serde_json::to_value(TransactionResponse::from(tx))
            .expect("a transaction response always serializes")),
        None => Ok(Value::Null),
    }
}

/// A receipt exists only once a transaction has been mined. A transaction still
/// sitting in the mempool correctly returns `null` here, which is exactly how a
/// client polls for confirmation.
pub fn get_transaction_receipt_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<Value> {
    let hash = parse_hash(&params.one::<String>()?)?;

    let state = context.chain.read();
    match state.get_receipt(&hash) {
        Some(receipt) => Ok(
            serde_json::to_value(TransactionReceiptResponse::from(receipt))
                .expect("a receipt response always serializes"),
        ),
        None => Ok(Value::Null),
    }
}


pub fn get_balance_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let balance = match tag {
        // "pending" means "after everything in the mempool lands", which is what a
        // wallet wants before it builds the next transaction.
        BlockTag::Pending => state.get_pending_balance(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.balance)
                .unwrap_or(U256::ZERO)
        }
    };

    Ok(quantity(balance))
}

/// The account's nonce: how many transactions it has sent.
pub fn get_transaction_count_handler(
    params: Params<'_>,
    context: &RPCContext,
) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let nonce = match tag {
        BlockTag::Pending => state.get_pending_nonce(&address),
        _ => {
            let number = resolve_tag(&state, &tag)?;
            state
                .get_account_at(&address, number)
                .map(|account| account.nonce)
                .unwrap_or(0)
        }
    };

    Ok(quantity(nonce))
}


/// Accepts a signed transaction, validates it, and queues it for mining.
///
/// The response is the transaction hash, returned the moment the transaction is
/// *accepted* — not when it is mined. The client then polls
/// `eth_getTransactionReceipt` until a receipt appears, which is the standard
/// Ethereum confirmation flow.
pub fn send_raw_transaction_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let raw: String = params.one()?;
    let raw: Bytes = raw
        .parse()
        .map_err(|e| invalid_params(format!("raw transaction is not valid hex: {e}")))?;

    let hash = context
        .chain
        .submit_raw_transaction(&raw)
        .map_err(invalid_input)?;

    Ok(data_hash(&hash))
}


/// What a caller sent, plus the snapshot and block environment to run it in.
struct Simulation {
    db: BreejaDb,
    block: BlockContext,
    spec: CallSpec,
}

/// Parses `(request, tag)` and takes a snapshot of the state at that block.
///
/// The read lock is held only while copying; the EVM runs afterwards with no lock,
/// the same discipline as the miner, so a slow call cannot freeze the node.
fn prepare(params: Params<'_>, context: &RPCContext) -> Result<Simulation, ErrorObjectOwned> {
    let mut sequence = params.sequence();
    let request: CallRequest = sequence.next()?;
    let tag = sequence.optional_next::<BlockTag>()?.unwrap_or(BlockTag::Latest);

    let (db, block) = {
        let state = context.chain.read();
        let number = resolve_tag(&state, &tag)?;
        let db = BreejaDb::from_state_at(&state, number)
            .ok_or_else(|| invalid_input(format!("no state for block {number}")))?;
        let header = state
            .get_block(number)
            .ok_or_else(|| invalid_input(format!("block {number} does not exist")))?;
        (db, BlockContext::from_block(header))
    };

    // A missing gas limit defaults to the whole block; one above the block limit
    // is clamped to it, as nodes do, rather than refused.
    let gas = request
        .gas
        .map(|gas| u64::try_from(gas).unwrap_or(u64::MAX))
        .unwrap_or(CALL_GAS_CEILING)
        .min(CALL_GAS_CEILING);

    let spec = CallSpec {
        from: request.from.unwrap_or_default(),
        to: request.to,
        value: request.value.unwrap_or_default(),
        input: request.calldata(),
        gas,
    };
    Ok(Simulation { db, block, spec })
}

/// The error for a call that ran and reverted. Code 3 and the revert data in
/// `data` are what Ethereum clients (and ethers/viem) look for.
fn reverted(output: &Bytes) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        3,
        "execution reverted",
        Some(format!("0x{}", alloy::hex::encode(output))),
    )
}

/// Executes a read-only call: the same EVM as a real transaction, against a copy
/// of the state, with the result thrown away.
///
/// A revert or halt comes back as a JSON-RPC error, because to the caller the call
/// failed; a success returns the contract's output as hex.
pub fn call_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let Simulation {
        mut db,
        block,
        spec,
    } = prepare(params, context)?;

    match simulate_call(&mut db, &block, &spec).map_err(invalid_input)? {
        CallResult::Success { output, .. } => Ok(format!("0x{}", alloy::hex::encode(output))),
        CallResult::Revert { output, .. } => Err(reverted(&output)),
        CallResult::Halt { reason, .. } => Err(invalid_input(format!("execution halted: {reason}"))),
    }
}

/// Estimates the gas a transaction would need: the smallest limit with which it
/// succeeds, found by binary search, plus a small margin (see
/// [`ESTIMATE_MARGIN_PERCENT`]).
pub fn estimate_gas_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let Simulation {
        mut db,
        block,
        spec,
    } = prepare(params, context)?;

    match estimate_gas(&mut db, &block, &spec) {
        Ok(gas) => Ok(quantity(gas)),
        Err(EstimateError::Fails(CallResult::Revert { output, .. })) => Err(reverted(&output)),
        Err(EstimateError::Fails(CallResult::Halt { reason, .. })) => Err(invalid_input(format!(
            "gas required exceeds allowance or execution halted: {reason}"
        ))),
        Err(EstimateError::Fails(CallResult::Success { .. })) => unreachable!("success is not a failure"),
        Err(EstimateError::Invalid(error)) => Err(invalid_input(error)),
    }
}

/// The code at an address: the deployed runtime bytecode, or `0x` if there is none.
pub fn get_code_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    let code = state
        .get_code_at(&address, number)
        .ok_or_else(|| invalid_input(format!("block {number} does not exist")))?;
    Ok(data_bytes(&code))
}

/// One storage slot of a contract, as a full 32-byte word (not a minimal
/// quantity: storage is raw data, so `42` is `0x00...2a`).
pub fn get_storage_at_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<String> {
    let mut sequence = params.sequence();
    let address: Address = sequence.next()?;
    let slot: U256 = sequence.next()?;
    let tag = sequence
        .optional_next::<BlockTag>()?
        .unwrap_or(BlockTag::Latest);

    let state = context.chain.read();
    let number = resolve_tag(&state, &tag)?;
    let value = state
        .get_storage_at(&address, slot, number)
        .ok_or_else(|| invalid_input(format!("block {number} does not exist")))?;
    Ok(format!("0x{}", alloy::hex::encode(value.to_be_bytes::<32>())))
}

/// The most blocks one `eth_getLogs` request may scan, so a careless
/// `fromBlock: 0` against a long chain cannot pin the server.
const MAX_LOG_BLOCK_RANGE: u64 = 10_000;

/// Logs matching a filter: a block range (or one `blockHash`), optional addresses,
/// and positional topics where `null` is a wildcard.
pub fn get_logs_handler(params: Params<'_>, context: &RPCContext) -> RpcResult<Vec<Value>> {
    let mut sequence = params.sequence();
    let request: LogFilterRequest = sequence.next()?;

    let state = context.chain.read();
    let (from_block, to_block) = match request.block_hash {
        Some(hash) => {
            let block = state
                .get_block_by_hash(&hash)
                .ok_or_else(|| invalid_input(format!("unknown block {hash}")))?;
            (block.number, block.number)
        }
        None => {
            let from = resolve_tag(&state, &request.from_block.unwrap_or(BlockTag::Latest))?;
            let to = resolve_tag(&state, &request.to_block.unwrap_or(BlockTag::Latest))?;
            (from, to)
        }
    };
    if from_block > to_block {
        return Err(invalid_input(format!(
            "fromBlock {from_block} is after toBlock {to_block}"
        )));
    }
    if to_block - from_block > MAX_LOG_BLOCK_RANGE {
        return Err(invalid_input(format!(
            "block range too large: at most {MAX_LOG_BLOCK_RANGE} blocks per request"
        )));
    }

    let filter = LogFilter {
        from_block,
        to_block,
        addresses: request.address.map(|a| a.into_vec()).unwrap_or_default(),
        topics: request
            .topics
            .unwrap_or_default()
            .into_iter()
            .map(|position| position.map(|options| options.into_vec()))
            .collect(),
    };

    Ok(state
        .get_logs(&filter)
        .into_iter()
        .map(|(receipt, log)| log_json(receipt, log))
        .collect())
}

/// The gas price. Breeja has one fixed price, so this is a constant: a wallet that
/// asks for a price to sign with gets the only one that will be accepted.
pub fn gas_price_handler() -> String {
    quantity(GAS_PRICE)
}
