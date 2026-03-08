// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::{
	block::{Block, BlockHeader},
	hash::Hash256,
	transaction::{OutPoint, Transaction, TxIn, TxOut},
};
use ironcat::validation::block::check_block;

const fn dummy_header() -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 1_387_838_302,
		bits: 0x1e0f_fff0,
		nonce: 0,
	}
}

fn coinbase_tx() -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig: vec![0x04, 0xff, 0xff, 0x00, 0x1d],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_0000_0000,
			script_pubkey: vec![0x76, 0xa9],
		}],
		locktime: 0,
	}
}

fn regular_tx() -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint {
				txid: Hash256::from_bytes([0xAA; 32]),
				index: 0,
			},
			script_sig: vec![0x48, 0x30],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 1_0000_0000,
			script_pubkey: vec![0x76, 0xa9],
		}],
		locktime: 0,
	}
}

#[test]
fn valid_block_with_coinbase_only() {
	let block = Block {
		header: dummy_header(),
		transactions: vec![coinbase_tx()],
	};
	assert!(check_block(&block).is_ok());
}

#[test]
fn valid_block_with_coinbase_and_regular() {
	let block = Block {
		header: dummy_header(),
		transactions: vec![coinbase_tx(), regular_tx()],
	};
	assert!(check_block(&block).is_ok());
}

#[test]
fn reject_empty_block() {
	let block = Block {
		header: dummy_header(),
		transactions: vec![],
	};
	let err = check_block(&block).unwrap_err();
	assert!(err.to_string().contains("no transactions"), "unexpected error: {err}");
}

#[test]
fn reject_first_tx_not_coinbase() {
	let block = Block {
		header: dummy_header(),
		transactions: vec![regular_tx()],
	};
	let err = check_block(&block).unwrap_err();
	assert!(
		err.to_string().contains("first transaction is not coinbase"),
		"unexpected error: {err}"
	);
}

#[test]
fn reject_multiple_coinbases() {
	let block = Block {
		header: dummy_header(),
		transactions: vec![coinbase_tx(), coinbase_tx()],
	};
	let err = check_block(&block).unwrap_err();
	assert!(err.to_string().contains("multiple coinbase"), "unexpected error: {err}");
}

#[test]
fn reject_block_exceeding_weight() {
	// A tx with a 1_000_001 byte script_pubkey produces a block well over
	// the 4M weight limit (serialized size * 4)
	let big_tx = Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig: vec![0x04, 0xff, 0xff, 0x00, 0x1d],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_0000_0000,
			script_pubkey: vec![0u8; 1_000_001],
		}],
		locktime: 0,
	};
	let block = Block {
		header: dummy_header(),
		transactions: vec![big_tx],
	};
	let err = check_block(&block).unwrap_err();
	assert!(err.to_string().contains("weight"), "unexpected error: {err}");
}
