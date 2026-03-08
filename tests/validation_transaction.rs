// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};
use ironcat::validation::subsidy::MAX_MONEY;
use ironcat::validation::transaction::check_transaction;

/// Helper: builds a minimal valid coinbase transaction
fn make_coinbase(scriptsig: Vec<u8>, value: i64) -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig: scriptsig,
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9], // dummy scriptPubKey
		}],
		locktime: 0,
	}
}

/// Helper: builds a minimal valid regular transaction
const fn make_regular_tx(inputs: Vec<TxIn>, outputs: Vec<TxOut>) -> Transaction {
	Transaction {
		version: 1,
		vin: inputs,
		vout: outputs,
		locktime: 0,
	}
}

/// Helper: creates a non-coinbase `TxIn` with a given txid byte and index
fn make_input(txid_byte: u8, index: u32) -> TxIn {
	let mut hash_bytes = [0u8; 32];
	hash_bytes[0] = txid_byte;
	TxIn {
		prev_output: OutPoint {
			txid: Hash256::from_bytes(hash_bytes),
			index,
		},
		script_sig: vec![0x00],
		sequence: 0xFFFF_FFFF,
	}
}

fn make_output(value: i64) -> TxOut {
	TxOut {
		value,
		script_pubkey: vec![0x76, 0xa9],
	}
}

#[test]
fn valid_coinbase() {
	let tx = make_coinbase(vec![0x04, 0xFF, 0xFF, 0x00], 50_0000_0000);
	assert!(check_transaction(&tx).is_ok());
}

#[test]
fn valid_regular_tx() {
	let tx = make_regular_tx(vec![make_input(1, 0)], vec![make_output(1_0000_0000)]);
	assert!(check_transaction(&tx).is_ok());
}

#[test]
fn reject_empty_inputs() {
	let tx = make_regular_tx(vec![], vec![make_output(1000)]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("no inputs"), "expected 'no inputs', got: {msg}");
}

#[test]
fn reject_empty_outputs() {
	let tx = make_regular_tx(vec![make_input(1, 0)], vec![]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("no outputs"), "expected 'no outputs', got: {msg}");
}

#[test]
fn reject_negative_output() {
	let tx = make_regular_tx(vec![make_input(1, 0)], vec![make_output(-1)]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("negative"), "expected 'negative', got: {msg}");
}

#[test]
fn reject_output_exceeds_max_money() {
	let tx = make_regular_tx(vec![make_input(1, 0)], vec![make_output(MAX_MONEY + 1)]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string();
	assert!(msg.contains("MAX_MONEY"), "expected 'MAX_MONEY', got: {msg}");
}

#[test]
fn reject_output_sum_overflow() {
	let tx = make_regular_tx(vec![make_input(1, 0)], vec![make_output(MAX_MONEY), make_output(1)]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string();
	assert!(msg.contains("MAX_MONEY"), "expected 'MAX_MONEY', got: {msg}");
}

#[test]
fn reject_duplicate_inputs() {
	let tx = make_regular_tx(vec![make_input(1, 0), make_input(1, 0)], vec![make_output(1000)]);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("duplicate"), "expected 'duplicate', got: {msg}");
}

#[test]
fn reject_coinbase_scriptsig_too_short() {
	let tx = make_coinbase(vec![0x04], 50_0000_0000);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("scriptsig"), "expected 'scriptsig', got: {msg}");
}

#[test]
fn reject_coinbase_scriptsig_too_long() {
	let tx = make_coinbase(vec![0x00; 101], 50_0000_0000);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("scriptsig"), "expected 'scriptsig', got: {msg}");
}

#[test]
fn reject_non_coinbase_with_null_prevout() {
	// Two inputs, first one is COINBASE outpoint -- not valid for non-coinbase tx
	let tx = make_regular_tx(
		vec![
			TxIn {
				prev_output: OutPoint::COINBASE,
				script_sig: vec![0x00],
				sequence: 0xFFFF_FFFF,
			},
			make_input(1, 0),
		],
		vec![make_output(1000)],
	);
	let err = check_transaction(&tx).unwrap_err();
	let msg = err.to_string().to_lowercase();
	assert!(msg.contains("null prevout"), "expected 'null prevout', got: {msg}");
}

#[test]
fn coinbase_scriptsig_at_boundaries() {
	// 2-byte scriptSig (minimum)
	let tx_min = make_coinbase(vec![0x01, 0x02], 50_0000_0000);
	assert!(check_transaction(&tx_min).is_ok(), "2-byte scriptsig should be valid");

	// 100-byte scriptSig (maximum)
	let tx_max = make_coinbase(vec![0x00; 100], 50_0000_0000);
	assert!(check_transaction(&tx_max).is_ok(), "100-byte scriptsig should be valid");
}
