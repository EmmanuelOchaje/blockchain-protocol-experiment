use std::time::Duration;

use alloy::primitives::{Address, U256};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use node::{
    Chain, ChainConfig, miner,
    testing::{DEV_PRIVATE_KEY, DEV_PRIVATE_KEY_1, address_of, one_eth, sign_call, sign_create, sign_transfer},
};
use primitives::node::BreejaGenesis;
use rpc::{RPCContext, start_server};
use serde_json::Value;

/// A running node and a client pointed at it.
struct Harness {
    client: HttpClient,
    chain: Chain,
    _server: jsonrpsee::server::ServerHandle,
    miner: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Starts a node with the given funded accounts, mining on a dedicated thread.
    async fn start(funded: &[(Address, U256)], mine: bool) -> Self {
        let config = ChainConfig {
            difficulty: 4,
            target_block_time: Duration::from_millis(50),
            miner: Address::repeat_byte(0x99),
        };
        let chain = Chain::new(
            BreejaGenesis {
                accounts: funded.to_vec(),
            },
            config,
        );

        // Port 0 lets the OS pick a free port, so tests can run in parallel.
        let (server, address) = start_server(RPCContext::new(chain.clone()), "127.0.0.1:0")
            .await
            .expect("server should start");
        let url = format!("http://{address}");
        let client = HttpClientBuilder::default().build(url).unwrap();

        let miner = mine.then(|| miner::spawn(chain.clone()));

        Self {
            client,
            chain,
            _server: server,
            miner,
        }
    }

    /// Polls `predicate` until it holds, or panics.
    async fn wait_for(&self, label: &str, mut predicate: impl FnMut(&Chain) -> bool) {
        for _ in 0..1_000 {
            if predicate(&self.chain) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {label}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.chain.shutdown();
        if let Some(handle) = self.miner.take() {
            let _ = handle.join();
        }
    }
}

/// Parses a hex quantity the way a client would.
fn from_quantity(raw: &str) -> u128 {
    u128::from_str_radix(raw.trim_start_matches("0x"), 16).expect("a valid hex quantity")
}

#[tokio::test]
async fn eth_chain_id_returns_the_configured_chain_id() {
    let harness = Harness::start(&[], false).await;
    let id: String = harness
        .client
        .request("eth_chainId", rpc_params![])
        .await
        .unwrap();
    assert_eq!(id, "0x1");
}

#[tokio::test]
async fn eth_block_number_starts_at_genesis_and_climbs_as_blocks_are_mined() {
    let harness = Harness::start(&[], true).await;

    harness
        .wait_for("blocks to be mined", |c| c.read().get_block_number() >= 3)
        .await;

    let number: String = harness
        .client
        .request("eth_blockNumber", rpc_params![])
        .await
        .unwrap();
    assert!(
        number.starts_with("0x"),
        "must be a hex quantity, got {number}"
    );
    assert!(from_quantity(&number) >= 3);
}

#[tokio::test]
async fn eth_get_block_by_number_accepts_tags_and_numbers() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 2)
        .await;

    // By tag, with the fullTransactions flag.
    let latest: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["latest", false])
        .await
        .unwrap();
    assert!(latest["hash"].as_str().unwrap().starts_with("0x"));

    // By tag, with the flag omitted entirely — also valid JSON-RPC.
    let genesis: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["earliest"])
        .await
        .unwrap();
    assert_eq!(genesis["number"], "0x0");
    assert_eq!(
        genesis["parentHash"],
        format!("0x{}", "0".repeat(64)),
        "genesis has no parent"
    );

    // By number.
    let first: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    assert_eq!(first["number"], "0x1");

    // Every field a client expects is present and hex-encoded.
    for field in [
        "number",
        "hash",
        "parentHash",
        "stateRoot",
        "transactionsRoot",
        "receiptsRoot",
        "difficulty",
        "totalDifficulty",
        "timestamp",
        "gasLimit",
        "gasUsed",
        "miner",
        "extraData",
        "nonce",
        "size",
    ] {
        let value = first[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        assert!(value.starts_with("0x"), "{field} is not hex: {value}");
    }
    // The proof-of-work nonce is a fixed-width 8-byte field, not a quantity.
    assert_eq!(first["nonce"].as_str().unwrap().len(), 18);
}

#[tokio::test]
async fn eth_get_block_by_number_rejects_a_block_that_does_not_exist() {
    let harness = Harness::start(&[], false).await;
    let result: Result<Value, _> = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0xffffff", false])
        .await;
    assert!(
        result.is_err(),
        "a future block should be an error, not null"
    );
}

#[tokio::test]
async fn eth_get_block_by_hash_finds_the_same_block_as_by_number() {
    let harness = Harness::start(&[], true).await;
    harness
        .wait_for("a block", |c| c.read().get_block_number() >= 1)
        .await;

    let by_number: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x1", false])
        .await
        .unwrap();
    let hash = by_number["hash"].as_str().unwrap().to_owned();

    let by_hash: Value = harness
        .client
        .request("eth_getBlockByHash", rpc_params![hash, false])
        .await
        .unwrap();
    assert_eq!(by_hash, by_number);

    // An unknown hash is null, not an error.
    let missing: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![format!("0x{}", "11".repeat(32)), false],
        )
        .await
        .unwrap();
    assert!(missing.is_null());
}

#[tokio::test]
async fn eth_get_balance_reads_current_pending_and_historical_state() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    // Latest, and the one-parameter form.
    let balance: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(balance, "0x56bc75e2d63100000");
    let without_tag: String = harness
        .client
        .request("eth_getBalance", rpc_params![alice])
        .await
        .unwrap();
    assert_eq!(without_tag, balance);

    // An address nobody funded is zero, not an error.
    let empty: String = harness
        .client
        .request(
            "eth_getBalance",
            rpc_params![Address::repeat_byte(0x77), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(empty, "0x0");

    // Send 5 ETH and wait for it to land.
    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth() * U256::from(5u64), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_account(&bob).balance > U256::ZERO
        })
        .await;

    let bob_now: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "latest"])
        .await
        .unwrap();
    assert_eq!(from_quantity(&bob_now), 5 * 10u128.pow(18));

    // At genesis Bob had nothing, and the chain still remembers that.
    let bob_at_genesis: String = harness
        .client
        .request("eth_getBalance", rpc_params![bob, "0x0"])
        .await
        .unwrap();
    assert_eq!(bob_at_genesis, "0x0");

    let _ = tx_hash;
}

#[tokio::test]
async fn eth_get_transaction_count_tracks_mined_and_pending_nonces() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    // No miner: the transactions stay pending, which is what this test is about.
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], false).await;

    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    for nonce in 0..2 {
        let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), nonce).unwrap();
        let _: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
    }

    // "latest" still sees no mined transactions...
    let mined: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "latest"])
        .await
        .unwrap();
    assert_eq!(mined, "0x0");

    // ...but "pending" includes the queued ones, which is the nonce a wallet must
    // use for its next transaction.
    let pending: String = harness
        .client
        .request("eth_getTransactionCount", rpc_params![alice, "pending"])
        .await
        .unwrap();
    assert_eq!(pending, "0x2");
}

#[tokio::test]
async fn a_transfer_produces_a_transaction_and_a_receipt() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let bob = address_of(DEV_PRIVATE_KEY_1).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    let raw = sign_transfer(DEV_PRIVATE_KEY, bob, one_eth(), 0).unwrap();
    let tx_hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    assert_eq!(tx_hash.len(), 66, "a transaction hash is 32 bytes of hex");

    // Before it is mined, there is a transaction but no receipt. That difference
    // is exactly how a client knows to keep waiting.
    harness
        .wait_for("the transfer to be mined", |c| {
            c.read().get_receipt(&tx_hash.parse().unwrap()).is_some()
        })
        .await;

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(tx["from"], format!("{alice:#x}"));
    assert_eq!(tx["to"], format!("{bob:#x}"));
    assert_eq!(tx["value"], "0xde0b6b3a7640000");
    assert_eq!(tx["nonce"], "0x0");
    assert_eq!(tx["input"], "0x");
    assert_eq!(tx["type"], "0x0", "legacy transaction");
    assert_eq!(tx["chainId"], "0x1");

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x1");
    assert_eq!(receipt["gasUsed"], "0x5208");
    assert_eq!(receipt["from"], format!("{alice:#x}"));
    assert_eq!(receipt["to"], format!("{bob:#x}"));
    assert!(
        receipt["contractAddress"].is_null(),
        "a plain transfer deploys nothing"
    );
    assert_eq!(receipt["logs"].as_array().unwrap().len(), 0);
    assert_eq!(receipt["blockHash"], tx["blockHash"]);

    // The block it landed in lists it, and can inline it.
    let block: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), true],
        )
        .await
        .unwrap();
    let inlined = block["transactions"].as_array().unwrap();
    assert_eq!(inlined.len(), 1);
    assert_eq!(inlined[0]["hash"], tx_hash);

    // With the flag off, the same block lists bare hashes instead.
    let hashes_only: Value = harness
        .client
        .request(
            "eth_getBlockByHash",
            rpc_params![receipt["blockHash"].as_str().unwrap(), false],
        )
        .await
        .unwrap();
    assert_eq!(hashes_only["transactions"][0], tx_hash);
}

#[tokio::test]
async fn an_unknown_transaction_or_receipt_is_null() {
    let harness = Harness::start(&[], false).await;
    let unknown = format!("0x{}", "11".repeat(32));

    let tx: Value = harness
        .client
        .request("eth_getTransactionByHash", rpc_params![unknown.clone()])
        .await
        .unwrap();
    assert!(tx.is_null());

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![unknown])
        .await
        .unwrap();
    assert!(receipt.is_null());
}

#[tokio::test]
async fn eth_send_raw_transaction_rejects_what_the_node_cannot_execute() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth())], false).await;

    // Not hex at all.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["not hex"])
        .await;
    assert!(result.is_err());

    // Hex, but not a transaction.
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params!["0xdeadbeef"])
        .await;
    assert!(result.is_err());

    // A real transaction for more ETH than the sender has.
    let raw = sign_transfer(
        DEV_PRIVATE_KEY,
        Address::repeat_byte(0x22),
        one_eth() * U256::from(1_000u64),
        0,
    )
    .unwrap();
    let result: Result<String, _> = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await;
    let error = result.expect_err("an unaffordable transfer must be rejected");
    assert!(
        error.to_string().contains("insufficient funds"),
        "the error should say why: {error}"
    );
}

/// Returns the constant 42 as a 32-byte word. Init code (12 bytes) + runtime (10).
const RETURNS_42_INIT: [u8; 22] = alloy::hex!("600a600c600039600a6000f3602a60005260206000f3");
/// Stores 42 in slot 0 and emits an event.
const STORE_42_INIT: [u8; 23] = alloy::hex!("600b600c600039600b6000f3602a60005560006000a000");
/// Always reverts, with no data.
const REVERTS_INIT: [u8; 17] = alloy::hex!("6005600c60003960056000f360006000fd");

/// Deploys `init` from the first dev account at `nonce` and returns the contract
/// address once it is mined.
async fn deploy(harness: &Harness, init: &[u8], nonce: u64) -> Address {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let raw = sign_create(DEV_PRIVATE_KEY, init.to_vec().into(), nonce, 200_000).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the deployment to be mined", |c| {
            c.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    alice.create(nonce)
}

#[tokio::test]
async fn eth_call_runs_contract_code_and_changes_nothing() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let returns_42 = deploy(&harness, &RETURNS_42_INIT, 0).await;
    let store_42 = deploy(&harness, &STORE_42_INIT, 1).await;

    // Called from an address that has never held any ETH: the checks are off.
    let nobody = Address::repeat_byte(0x77);
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"from": nobody, "to": returns_42}), "latest"],
        )
        .await
        .unwrap();
    assert_eq!(result, format!("0x{}2a", "00".repeat(31)));

    // A call that writes storage reports success, but the write is discarded.
    let result: String = harness
        .client
        .request("eth_call", rpc_params![serde_json::json!({"to": store_42})])
        .await
        .unwrap();
    assert_eq!(result, "0x");
    assert_eq!(harness.chain.read().get_storage(&store_42, U256::ZERO), U256::ZERO);
    assert_eq!(harness.chain.read().get_account(&nobody).nonce, 0);

    // A plain transfer-shaped call returns no data.
    let result: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": Address::repeat_byte(0x22), "value": "0x1"})],
        )
        .await
        .unwrap();
    assert_eq!(result, "0x");
}

#[tokio::test]
async fn eth_call_reports_a_revert_as_an_error_and_can_read_the_past() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let reverts = deploy(&harness, &REVERTS_INIT, 0).await;

    let result: Result<String, _> = harness
        .client
        .request("eth_call", rpc_params![serde_json::json!({"to": reverts})])
        .await;
    let error = result.expect_err("a reverting call is an error");
    assert!(error.to_string().contains("execution reverted"), "got: {error}");
    assert!(error.to_string().contains("3"), "code 3: {error}");

    // At block 0 the contract does not exist yet, so calling it runs no code.
    let result: String = harness
        .client
        .request("eth_call", rpc_params![serde_json::json!({"to": reverts}), "0x0"])
        .await
        .unwrap();
    assert_eq!(result, "0x");

    // A block that does not exist is an error, not a silent fallback to latest.
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": reverts}), "0xffffff"],
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn eth_estimate_gas_finds_the_gas_a_transaction_needs() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let store_42 = deploy(&harness, &STORE_42_INIT, 0).await;
    let reverts = deploy(&harness, &REVERTS_INIT, 1).await;

    // A plain transfer is exactly the intrinsic cost: no margin.
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({
                "from": alice, "to": Address::repeat_byte(0x22), "value": "0x1"
            })],
        )
        .await
        .unwrap();
    assert_eq!(estimate, "0x5208");

    // A contract call needs at least what it uses (43,487) and gets a small margin.
    let estimate: String = harness
        .client
        .request("eth_estimateGas", rpc_params![serde_json::json!({"from": alice, "to": store_42})])
        .await
        .unwrap();
    let gas = from_quantity(&estimate);
    assert!((43_487..=43_487 * 110 / 100).contains(&gas), "got {gas}");

    // A deployment (no `to`) is estimated too.
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "data": format!("0x{}", alloy::hex::encode(STORE_42_INIT))})],
        )
        .await
        .unwrap();
    assert!(from_quantity(&estimate) >= 55_522);

    // A call that always reverts cannot be estimated: no gas limit would help.
    let result: Result<String, _> = harness
        .client
        .request("eth_estimateGas", rpc_params![serde_json::json!({"from": alice, "to": reverts})])
        .await;
    assert!(result.expect_err("must fail").to_string().contains("execution reverted"));

    // A gas ceiling below the intrinsic cost is refused outright.
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "to": store_42, "gas": "0x100"})],
        )
        .await;
    assert!(result.is_err());
}

/// Stores 42 in slot 0 and emits `LOG1` with topic `0x..aa`.
const LOGS_WITH_TOPIC_INIT: [u8; 25] = alloy::hex!("600d600c600039600d6000f3602a60005560aa60006000a100");

fn topic_aa() -> String {
    format!("0x{}aa", "00".repeat(31))
}

/// Sends a call to `to` from the first dev account and waits for it to be mined.
async fn send_call(harness: &Harness, to: Address, nonce: u64) -> String {
    let raw = sign_call(DEV_PRIVATE_KEY, to, Default::default(), nonce, 100_000).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the call to be mined", |c| {
            c.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    hash
}

#[tokio::test]
async fn eth_get_code_and_eth_get_storage_at_read_a_deployed_contract() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let contract = deploy(&harness, &STORE_42_INIT, 0).await;
    send_call(&harness, contract, 1).await;

    // The runtime code, not the init code.
    let code: String = harness
        .client
        .request("eth_getCode", rpc_params![contract, "latest"])
        .await
        .unwrap();
    assert_eq!(code, "0x602a60005560006000a000");

    // An account with no code, and an address nobody has used, read as 0x.
    for address in [alice, Address::repeat_byte(0x77)] {
        let code: String = harness
            .client
            .request("eth_getCode", rpc_params![address, "latest"])
            .await
            .unwrap();
        assert_eq!(code, "0x");
    }
    // Before the deployment, the contract's address had no code.
    let code: String = harness
        .client
        .request("eth_getCode", rpc_params![contract, "0x0"])
        .await
        .unwrap();
    assert_eq!(code, "0x");

    // Slot 0 holds 42, as a full 32-byte word.
    let slot: String = harness
        .client
        .request("eth_getStorageAt", rpc_params![contract, "0x0", "latest"])
        .await
        .unwrap();
    assert_eq!(slot, format!("0x{}2a", "00".repeat(31)));
    // An unwritten slot, and slot 0 before the call, are zero.
    let zero = format!("0x{}", "00".repeat(32));
    for (slot, block) in [("0x1", "latest"), ("0x0", "0x0")] {
        let value: String = harness
            .client
            .request("eth_getStorageAt", rpc_params![contract, slot, block])
            .await
            .unwrap();
        assert_eq!(value, zero);
    }
    // A block that does not exist is an error.
    let result: Result<String, _> = harness
        .client
        .request("eth_getStorageAt", rpc_params![contract, "0x0", "0xffffff"])
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn eth_get_logs_filters_by_range_address_and_topics() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let contract = deploy(&harness, &LOGS_WITH_TOPIC_INIT, 0).await;
    let tx_hash = send_call(&harness, contract, 1).await;

    let logs = |filter: Value| {
        let client = harness.client.clone();
        async move {
            client
                .request::<Vec<Value>, _>("eth_getLogs", rpc_params![filter])
                .await
                .unwrap()
        }
    };

    // By address over the whole chain: exactly the one log the call emitted.
    let found = logs(serde_json::json!({"fromBlock": "0x0", "toBlock": "latest", "address": contract})).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["address"], format!("{contract:#x}"));
    assert_eq!(found[0]["transactionHash"], tx_hash);
    assert_eq!(found[0]["topics"][0], topic_aa());
    assert_eq!(found[0]["removed"], false);

    // An address list works too, and a different address finds nothing.
    let found = logs(serde_json::json!({"fromBlock": "0x0", "address": [contract, alice]})).await;
    assert_eq!(found.len(), 1);
    let found = logs(serde_json::json!({"fromBlock": "0x0", "address": alice})).await;
    assert!(found.is_empty());

    // By block range: the log's own block finds it, an earlier range does not.
    let block = found_block(&harness, &tx_hash).await;
    let found = logs(serde_json::json!({"fromBlock": block, "toBlock": block})).await;
    assert_eq!(found.len(), 1);
    let found = logs(serde_json::json!({"fromBlock": "0x0", "toBlock": "0x0"})).await;
    assert!(found.is_empty());

    // Topics are positional, and null is a wildcard.
    let hit = serde_json::json!({"fromBlock": "0x0", "topics": [topic_aa()]});
    assert_eq!(logs(hit).await.len(), 1);
    let hit = serde_json::json!({"fromBlock": "0x0", "topics": [[topic_aa(), format!("0x{}bb", "00".repeat(31))]]});
    assert_eq!(logs(hit).await.len(), 1, "OR within a position");
    let miss = serde_json::json!({"fromBlock": "0x0", "topics": [format!("0x{}bb", "00".repeat(31))]});
    assert!(logs(miss).await.is_empty());
    let miss = serde_json::json!({"fromBlock": "0x0", "topics": [null, topic_aa()]});
    assert!(logs(miss).await.is_empty(), "the log has no second topic");
    let wildcard = serde_json::json!({"fromBlock": "0x0", "topics": [null]});
    assert_eq!(logs(wildcard).await.len(), 1);

    // A reversed range is refused.
    let result: Result<Vec<Value>, _> = harness
        .client
        .request("eth_getLogs", rpc_params![serde_json::json!({"fromBlock": "0x5", "toBlock": "0x1"})])
        .await;
    assert!(result.is_err());
}

/// The block number (as hex) a mined transaction landed in.
async fn found_block(harness: &Harness, tx_hash: &str) -> String {
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash])
        .await
        .unwrap();
    receipt["blockNumber"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn logs_bloom_is_real_for_receipts_and_blocks() {
    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;
    let contract = deploy(&harness, &LOGS_WITH_TOPIC_INIT, 0).await;
    let tx_hash = send_call(&harness, contract, 1).await;
    let zero_bloom = format!("0x{}", "00".repeat(256));

    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![tx_hash.clone()])
        .await
        .unwrap();
    let bloom: alloy::primitives::Bloom = receipt["logsBloom"].as_str().unwrap().parse().unwrap();
    assert_ne!(receipt["logsBloom"], zero_bloom);
    // It matches the log: the emitter and the topic are in it, an outsider is not.
    assert!(primitives::node::bloom_may_contain(&bloom, contract.as_slice()));
    assert!(primitives::node::bloom_may_contain(&bloom, alloy::hex::decode(topic_aa()).unwrap().as_slice()));
    assert!(!primitives::node::bloom_may_contain(&bloom, alice.as_slice()));

    // The block that holds the call carries the same bloom.
    let block: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params![receipt["blockNumber"].clone(), false])
        .await
        .unwrap();
    assert_eq!(block["logsBloom"], receipt["logsBloom"]);

    // A block with no logs has an empty bloom.
    let genesis: Value = harness
        .client
        .request("eth_getBlockByNumber", rpc_params!["0x0", false])
        .await
        .unwrap();
    assert_eq!(genesis["logsBloom"], zero_bloom);
}

#[tokio::test]
async fn eth_gas_price_returns_the_one_fixed_price() {
    let harness = Harness::start(&[], false).await;
    let price: String = harness.client.request("eth_gasPrice", rpc_params![]).await.unwrap();
    assert_eq!(price, "0x3b9aca00", "1 gwei");
}

/// The deliverable-4 demo: a real Solidity contract (examples/Counter.sol, compiled
/// by solc 0.8.24 for Shanghai) is deployed and used end to end over JSON-RPC.
#[tokio::test]
async fn a_solidity_contract_can_be_deployed_called_estimated_logged_and_reverted() {
    use alloy::primitives::keccak256;

    let alice = address_of(DEV_PRIVATE_KEY).unwrap();
    let harness = Harness::start(&[(alice, one_eth() * U256::from(100u64))], true).await;

    let creation = alloy::hex::decode(include_str!("../../../examples/Counter.bin").trim()).unwrap();
    let runtime = include_str!("../../../examples/Counter.runtime.bin").trim().to_owned();
    let selector = |signature: &str| keccak256(signature.as_bytes())[..4].to_vec();
    let word = |value: u64| U256::from(value).to_be_bytes::<32>().to_vec();

    // 1. Deploy. Solc emits PUSH0, so this only works under Shanghai rules.
    assert!(creation.windows(3).any(|w| w == [0x5f, 0x80, 0xfd]), "uses PUSH0");
    let counter = deploy(&harness, &creation, 0).await;
    let code: String = harness
        .client
        .request("eth_getCode", rpc_params![counter, "latest"])
        .await
        .unwrap();
    assert_eq!(code, format!("0x{runtime}"), "the runtime code is what is stored");

    // 2. Send state-changing calls, using the node's own estimate as the gas limit.
    let calldata = format!("0x{}", alloy::hex::encode(selector("increment()")));
    let estimate: String = harness
        .client
        .request(
            "eth_estimateGas",
            rpc_params![serde_json::json!({"from": alice, "to": counter, "data": calldata})],
        )
        .await
        .unwrap();
    let gas = from_quantity(&estimate) as u64;
    for nonce in 1..=2 {
        let raw = sign_call(DEV_PRIVATE_KEY, counter, selector("increment()").into(), nonce, gas).unwrap();
        let hash: String = harness
            .client
            .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
            .await
            .unwrap();
        harness
            .wait_for("increment to be mined", |c| {
                c.read().get_receipt(&hash.parse().unwrap()).is_some()
            })
            .await;
        let receipt: Value = harness
            .client
            .request("eth_getTransactionReceipt", rpc_params![hash])
            .await
            .unwrap();
        assert_eq!(receipt["status"], "0x1", "the estimate was enough gas");
    }

    // 3. Read it back three ways: eth_call, eth_getStorageAt, and events.
    let count: String = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": counter, "data": format!("0x{}", alloy::hex::encode(selector("count()")))})],
        )
        .await
        .unwrap();
    assert_eq!(count, format!("0x{}", alloy::hex::encode(word(2))));
    let slot: String = harness
        .client
        .request("eth_getStorageAt", rpc_params![counter, "0x0", "latest"])
        .await
        .unwrap();
    assert_eq!(slot, count);

    let topic0 = format!("{:#x}", keccak256("Incremented(address,uint256)".as_bytes()));
    let by_alice = format!("0x{}{}", "00".repeat(12), alloy::hex::encode(alice));
    let logs: Vec<Value> = harness
        .client
        .request(
            "eth_getLogs",
            rpc_params![serde_json::json!({"fromBlock": "0x0", "address": counter, "topics": [topic0, by_alice]})],
        )
        .await
        .unwrap();
    assert_eq!(logs.len(), 2, "one Incremented event per increment");
    assert_eq!(logs[0]["data"], format!("0x{}", alloy::hex::encode(word(1))));
    assert_eq!(logs[1]["data"], format!("0x{}", alloy::hex::encode(word(2))));

    // 4. A revert: set(500) hits the custom error. eth_call reports it with the
    //    error selector as data, and a real transaction is mined with status 0.
    let mut too_big = selector("set(uint256)");
    too_big.extend(word(500));
    let result: Result<String, _> = harness
        .client
        .request(
            "eth_call",
            rpc_params![serde_json::json!({"to": counter, "data": format!("0x{}", alloy::hex::encode(&too_big))})],
        )
        .await;
    let error = result.expect_err("the call reverts").to_string();
    assert!(error.contains("execution reverted"), "got: {error}");
    assert!(error.contains(&alloy::hex::encode(selector("TooBig()"))), "carries the TooBig() selector: {error}");

    let balance_before = harness.chain.read().get_account(&alice).balance;
    let raw = sign_call(DEV_PRIVATE_KEY, counter, too_big.into(), 3, 100_000).unwrap();
    let hash: String = harness
        .client
        .request("eth_sendRawTransaction", rpc_params![raw.to_string()])
        .await
        .unwrap();
    harness
        .wait_for("the reverting call to be mined", |c| {
            c.read().get_receipt(&hash.parse().unwrap()).is_some()
        })
        .await;
    let receipt: Value = harness
        .client
        .request("eth_getTransactionReceipt", rpc_params![hash])
        .await
        .unwrap();
    assert_eq!(receipt["status"], "0x0");
    assert!(harness.chain.read().get_account(&alice).balance < balance_before, "the sender still paid");
    assert_eq!(harness.chain.read().get_account(&alice).nonce, 4, "and the nonce advanced");
    let after: String = harness
        .client
        .request("eth_getStorageAt", rpc_params![counter, "0x0", "latest"])
        .await
        .unwrap();
    assert_eq!(after, count, "the revert undid the write");
}

#[tokio::test]
async fn an_unsupported_method_gets_the_standard_method_not_found_error() {
    let harness = Harness::start(&[], false).await;

    // `eth_getProof` is a real Ethereum method this node does not implement, so it
    // must come back as -32601 rather than something invented.
    let result: Result<Value, _> = harness.client.request("eth_getProof", rpc_params![]).await;
    let error = result.expect_err("an unregistered method must be an error");
    assert!(
        error.to_string().contains("Method not found") || error.to_string().contains("-32601"),
        "got: {error}"
    );
}
