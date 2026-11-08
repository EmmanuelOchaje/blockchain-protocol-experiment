use alloy::consensus::transaction::SignerRecoverable;
use alloy::consensus::{Transaction as TxTrait, TxEnvelope, TxType};
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use primitives::{
    constants::{BLOCK_GAS_LIMIT, CHAIN_ID, GAS_PRICE},
    node::{BlockState, PendingTransaction, intrinsic_gas},
};

/// Largest init code a creation may carry (EIP-3860: twice the 24,576 byte code limit).
const MAX_INITCODE_SIZE: usize = 49_152;

/// A signed transaction that has been decoded and checked against the protocol
/// rules, but not yet against chain state.
struct DecodedTransaction {
    hash: B256,
    from: Address,
    to: Option<Address>,
    input: Bytes,
    value: U256,
    nonce: u64,
    gas: u64,
    gas_price: u128,
    chain_id: u64,
    v: u64,
    r: U256,
    s: U256,
}

/// Decodes a raw signed transaction and applies every rule that can be checked
/// without looking at chain state.
fn decode_and_check(raw: &Bytes) -> Result<DecodedTransaction, anyhow::Error> {
    if raw.is_empty() {
        anyhow::bail!("raw transaction is empty");
    }

    let envelope = TxEnvelope::decode_2718(&mut raw.as_ref())
        .map_err(|e| anyhow::anyhow!("could not RLP-decode the raw transaction: {e}"))?;

    // Breeja predates typed transactions, so it only understands legacy ones.
    // There is no base fee here for an EIP-1559 transaction to bid against.
    if !matches!(envelope.tx_type(), TxType::Legacy) {
        anyhow::bail!(
            "unsupported transaction type {:?}: this node only accepts legacy \
             (pre-EIP-1559) transactions, so sign with --legacy",
            envelope.tx_type()
        );
    }

    // The sender is derived from the signature, never declared.
    let from = envelope
        .recover_signer()
        .map_err(|e| anyhow::anyhow!("invalid signature, could not recover sender: {e}"))?;

    // EIP-155 replay protection: a transaction signed for another chain must not
    // be mineable here. A legacy transaction with no chain id at all predates
    // EIP-155 and is accepted, exactly as on Ethereum.
    let chain_id = match envelope.chain_id() {
        Some(id) if id == CHAIN_ID => id,
        Some(id) => {
            anyhow::bail!("transaction is signed for chain {id}, but this chain is {CHAIN_ID}")
        }
        None => CHAIN_ID,
    };

    // A creation has no recipient; its input is the init code.
    let to = match envelope.kind() {
        TxKind::Call(to) => Some(to),
        TxKind::Create => None,
    };
    let input = envelope.input().clone();

    // Init code is capped (EIP-3860, in force under Shanghai).
    if to.is_none() && input.len() > MAX_INITCODE_SIZE {
        anyhow::bail!(
            "init code is {} bytes, above the {MAX_INITCODE_SIZE} byte limit",
            input.len()
        );
    }

    // The gas limit must at least cover the intrinsic cost: 21,000 for a transfer,
    // more for calldata and for a creation. The EVM would refuse it anyway, but
    // refusing it here keeps it out of the mempool.
    let gas = envelope.gas_limit();
    let intrinsic = intrinsic_gas(&input, to.is_none());
    if gas < intrinsic {
        anyhow::bail!("gas limit {gas} is below the intrinsic cost of this transaction ({intrinsic})");
    }

    // A transaction whose limit exceeds a whole block could never be included, and
    // since blocks take transactions in order it would block everything behind it.
    if gas > BLOCK_GAS_LIMIT {
        anyhow::bail!("gas limit {gas} is above the block gas limit ({BLOCK_GAS_LIMIT})");
    }

    let gas_price = envelope.max_fee_per_gas();
    if gas_price < GAS_PRICE {
        anyhow::bail!("gas price {gas_price} is below this chain's fixed price of {GAS_PRICE}");
    }

    let signature = envelope.signature();
    Ok(DecodedTransaction {
        hash: *envelope.tx_hash(),
        from,
        to,
        input,
        value: envelope.value(),
        nonce: envelope.nonce(),
        gas,
        gas_price,
        chain_id,
        v: legacy_v(signature.v(), envelope.chain_id()),
        r: signature.r(),
        s: signature.s(),
    })
}

/// Reconstructs the `v` value a legacy transaction reports over JSON-RPC.
///
/// `v` encodes the signature's y-parity, and since EIP-155 it also encodes the
/// chain id, which is how replay protection is folded into the signature itself.
fn legacy_v(y_parity: bool, chain_id: Option<u64>) -> u64 {
    match chain_id {
        Some(id) => y_parity as u64 + 35 + id * 2,
        None => y_parity as u64 + 27,
    }
}

/// Validates a raw transaction against the current chain state and admits it to
/// the mempool, returning its hash.
///
/// The state checks use *pending* values — the nonce and balance including
/// everything already queued — so that a client can fire several transactions
/// back to back without waiting for a block in between.
pub fn admit(state: &mut BlockState, raw: &Bytes) -> Result<B256, anyhow::Error> {
    let tx = decode_and_check(raw)?;

    if state.get_transaction(&tx.hash).is_some() {
        anyhow::bail!("transaction {} has already been mined", tx.hash);
    }
    if state.mempool_contains(&tx.hash) {
        anyhow::bail!("transaction {} is already pending", tx.hash);
    }

    let expected_nonce = state.get_pending_nonce(&tx.from);
    if tx.nonce != expected_nonce {
        anyhow::bail!(
            "nonce too {}: {} expects nonce {expected_nonce}, transaction has {}",
            if tx.nonce < expected_nonce {
                "low"
            } else {
                "high"
            },
            tx.from,
            tx.nonce
        );
    }

    // Ethereum reserves the full gas limit up front and refunds whatever the
    // transaction does not burn. Breeja always burns exactly the intrinsic cost,
    // but it reserves the limit anyway so the accounting matches.
    let max_cost = tx.value + U256::from(tx.gas) * U256::from(tx.gas_price);
    let available = state.get_pending_balance(&tx.from);
    if available < max_cost {
        anyhow::bail!(
            "insufficient funds: {} has {available} available but the transaction \
             needs up to {max_cost} (value plus gas)",
            tx.from
        );
    }

    state.add_to_mempool(PendingTransaction {
        hash: tx.hash,
        from: tx.from,
        to: tx.to,
        input: tx.input,
        value: tx.value,
        nonce: tx.nonce,
        gas: tx.gas,
        gas_price: tx.gas_price,
        chain_id: tx.chain_id,
        v: tx.v,
        r: tx.r,
        s: tx.s,
    });

    Ok(tx.hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_v_encodes_parity_and_chain_id() {
        // Pre-EIP-155: 27 or 28.
        assert_eq!(legacy_v(false, None), 27);
        assert_eq!(legacy_v(true, None), 28);
        // EIP-155 on chain 1: 37 or 38.
        assert_eq!(legacy_v(false, Some(1)), 37);
        assert_eq!(legacy_v(true, Some(1)), 38);
    }

    #[test]
    fn garbage_bytes_are_rejected_not_panicked_on() {
        assert!(decode_and_check(&Bytes::new()).is_err());
        assert!(decode_and_check(&Bytes::from_static(&[0xff, 0x00, 0x13])).is_err());
    }

    #[test]
    fn a_creation_and_calldata_are_now_admissible() {
        use crate::testing::{DEV_PRIVATE_KEY, sign_call, sign_create};
        let create = sign_create(DEV_PRIVATE_KEY, vec![0x00].into(), 0, 100_000).unwrap();
        let decoded = decode_and_check(&create).unwrap();
        assert_eq!(decoded.to, None);

        let call = sign_call(DEV_PRIVATE_KEY, Address::ZERO, vec![1, 2, 3].into(), 0, 50_000).unwrap();
        assert_eq!(decode_and_check(&call).unwrap().input.len(), 3);
    }

    #[test]
    fn a_gas_limit_below_the_intrinsic_cost_is_rejected() {
        use crate::testing::{DEV_PRIVATE_KEY, sign_call, sign_create};
        // 21,000 covers a bare transfer but not 32 non-zero bytes of calldata.
        let call = sign_call(DEV_PRIVATE_KEY, Address::ZERO, vec![1; 32].into(), 0, 21_000).unwrap();
        assert!(decode_and_check(&call).is_err());
        // Nor does it cover a creation's extra 32,000.
        let create = sign_create(DEV_PRIVATE_KEY, vec![0].into(), 0, 40_000).unwrap();
        assert!(decode_and_check(&create).is_err());
    }

    #[test]
    fn a_gas_limit_above_the_block_gas_limit_is_rejected() {
        use crate::testing::{DEV_PRIVATE_KEY, sign_call};
        // It could never be included, and would block every transaction behind it.
        let huge = sign_call(DEV_PRIVATE_KEY, Address::ZERO, Default::default(), 0, BLOCK_GAS_LIMIT + 1).unwrap();
        let error = decode_and_check(&huge).err().expect("must be rejected");
        assert!(error.to_string().contains("block gas limit"), "got: {error}");
        let at_limit = sign_call(DEV_PRIVATE_KEY, Address::ZERO, Default::default(), 0, BLOCK_GAS_LIMIT).unwrap();
        assert!(decode_and_check(&at_limit).is_ok());
    }
}
