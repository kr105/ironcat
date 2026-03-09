// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing/arithmetic for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::cast_possible_wrap,
	clippy::arithmetic_side_effects
)]

use ironcat::chainstate::ChainState;
use ironcat::types::block::{Block, BlockHeader};
use ironcat::types::hash::{double_sha256, Hash256};
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};
use ironcat::validation::subsidy::{get_block_subsidy, COIN};

/// Creates a coinbase transaction with BIP34 height encoding in the `script_sig`
fn make_coinbase(height: u32, value: i64) -> Transaction {
	// BIP34: push the height as a minimally-encoded little-endian integer
	let mut script_sig = Vec::new();
	if height == 0 {
		// Special case: height 0 is encoded as OP_0 (0x00)
		script_sig.push(1);
		script_sig.push(0);
	} else if height <= 0xFF {
		script_sig.push(1);
		script_sig.push(height as u8);
	} else if height <= 0xFFFF {
		script_sig.push(2);
		script_sig.push(height as u8);
		script_sig.push((height >> 8) as u8);
	} else {
		script_sig.push(3);
		script_sig.push(height as u8);
		script_sig.push((height >> 8) as u8);
		script_sig.push((height >> 16) as u8);
	}

	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig,
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9, 0x14], // dummy P2PKH prefix
		}],
		locktime: 0,
	}
}

/// Creates a simple spending transaction with one input and one output
fn make_spend_tx(outpoint: OutPoint, value: i64) -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: outpoint,
			script_sig: vec![0x00], // dummy sig
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9, 0x14],
		}],
		locktime: 0,
	}
}

/// Creates a block with the given previous hash and transactions
fn make_block(prev_hash: Hash256, txs: Vec<Transaction>) -> Block {
	Block {
		header: BlockHeader {
			version: 1,
			prev_hash,
			merkle_root: Hash256::ZERO, // not validated by ChainState
			timestamp: 1_700_000_000,
			bits: 0x1d00_ffff,
			nonce: 0,
		},
		transactions: txs,
	}
}

/// Computes the block hash (double SHA-256 of the serialized header)
fn block_hash(block: &Block) -> Hash256 {
	double_sha256(&block.header.to_bytes())
}

fn open_chainstate() -> (tempfile::TempDir, ChainState) {
	let dir = tempfile::tempdir().unwrap();
	let cs = ChainState::open(dir.path(), u32::MAX).unwrap();
	(dir, cs)
}

/// Connects coinbase-only blocks from `from` through `to` (inclusive) to mature earlier coinbases
fn mature_coinbase(cs: &ChainState, prev: Hash256, from: u32, to: u32) -> Hash256 {
	let mut prev = prev;
	for h in from..=to {
		let block = make_block(prev, vec![make_coinbase(h, 50 * COIN)]);
		let h_hash = block_hash(&block);
		cs.connect_block(&block, h, &h_hash).unwrap();
		prev = h_hash;
	}
	prev
}

#[test]
fn connect_coinbase_only_block() {
	let (_dir, cs) = open_chainstate();

	let block = make_block(Hash256::ZERO, vec![make_coinbase(1, 50 * COIN)]);
	let hash = block_hash(&block);

	cs.connect_block(&block, 1, &hash).unwrap();
	assert_eq!(cs.tip(), hash);
}

#[test]
fn connect_and_spend() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase creates coins
	let coinbase_1 = make_coinbase(1, 50 * COIN);
	let coinbase_1_txid = coinbase_1.txid();
	let block_1 = make_block(Hash256::ZERO, vec![coinbase_1]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	let prev = mature_coinbase(&cs, hash_1, 2, 100);

	// Block 101: spend the coinbase from block 1
	let spend = make_spend_tx(
		OutPoint {
			txid: coinbase_1_txid,
			index: 0,
		},
		49 * COIN, // 1 COIN fee
	);
	let block_101 = make_block(prev, vec![make_coinbase(101, 50 * COIN + COIN), spend]);
	let hash_101 = block_hash(&block_101);

	cs.connect_block(&block_101, 101, &hash_101).unwrap();
	cs.fsync_undo().unwrap();
	assert_eq!(cs.tip(), hash_101);
}

#[test]
fn double_spend_rejected() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase
	let coinbase = make_coinbase(1, 50 * COIN);
	let coinbase_txid = coinbase.txid();
	let block_1 = make_block(Hash256::ZERO, vec![coinbase]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	let prev = mature_coinbase(&cs, hash_1, 2, 100);

	// Spend once at height 101
	let outpoint = OutPoint {
		txid: coinbase_txid,
		index: 0,
	};
	let spend_1 = make_spend_tx(outpoint.clone(), 49 * COIN);
	let block_101 = make_block(prev, vec![make_coinbase(101, 50 * COIN + COIN), spend_1]);
	let hash_101 = block_hash(&block_101);
	cs.connect_block(&block_101, 101, &hash_101).unwrap();

	// Try to spend the same outpoint again at height 102
	let spend_2 = make_spend_tx(outpoint, 49 * COIN);
	let block_102 = make_block(hash_101, vec![make_coinbase(102, 50 * COIN + COIN), spend_2]);
	let hash_102 = block_hash(&block_102);

	let result = cs.connect_block(&block_102, 102, &hash_102);
	assert!(result.is_err());
	assert!(
		result.unwrap_err().to_string().contains("missing utxo"),
		"expected missing utxo error for double spend"
	);
}

#[test]
fn coinbase_maturity_enforced() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase
	let coinbase = make_coinbase(1, 50 * COIN);
	let coinbase_txid = coinbase.txid();
	let block_1 = make_block(Hash256::ZERO, vec![coinbase]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	// Only mature to height 50 (not enough)
	let prev = mature_coinbase(&cs, hash_1, 2, 50);

	// Try to spend at height 51 (only 50 confirmations, need 100)
	let spend = make_spend_tx(
		OutPoint {
			txid: coinbase_txid,
			index: 0,
		},
		49 * COIN,
	);
	let immature_block = make_block(prev, vec![make_coinbase(51, 50 * COIN + COIN), spend]);
	let immature_hash = block_hash(&immature_block);

	let result = cs.connect_block(&immature_block, 51, &immature_hash);
	assert!(result.is_err());
	assert!(
		result.unwrap_err().to_string().contains("not mature"),
		"expected coinbase maturity error"
	);
}

#[test]
fn coinbase_overpay_rejected() {
	let (_dir, cs) = open_chainstate();

	let subsidy = get_block_subsidy(1);
	// Coinbase claims more than subsidy when there are no fees
	let block = make_block(Hash256::ZERO, vec![make_coinbase(1, subsidy + 1)]);
	let hash = block_hash(&block);

	let result = cs.connect_block(&block, 1, &hash);
	assert!(result.is_err());
	assert!(
		result.unwrap_err().to_string().contains("coinbase value"),
		"expected coinbase overpay error"
	);
}

#[test]
fn disconnect_restores_utxo() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase
	let coinbase = make_coinbase(1, 50 * COIN);
	let coinbase_txid = coinbase.txid();
	let block_1 = make_block(Hash256::ZERO, vec![coinbase]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	let prev = mature_coinbase(&cs, hash_1, 2, 100);

	// Block 101: spend the coinbase
	let outpoint = OutPoint {
		txid: coinbase_txid,
		index: 0,
	};
	let spend = make_spend_tx(outpoint.clone(), 49 * COIN);
	let block_101 = make_block(prev, vec![make_coinbase(101, 50 * COIN + COIN), spend]);
	let hash_101 = block_hash(&block_101);
	cs.connect_block(&block_101, 101, &hash_101).unwrap();
	cs.fsync_undo().unwrap();

	// Disconnect block 101
	cs.disconnect_block(&block_101, 101, &hash_101).unwrap();
	assert_eq!(cs.tip(), prev);

	// The UTXO should be restored, so we can spend it again
	let spend_again = make_spend_tx(outpoint, 48 * COIN);
	let block_101b = make_block(prev, vec![make_coinbase(101, 50 * COIN + 2 * COIN), spend_again]);
	let hash_101b = block_hash(&block_101b);
	cs.connect_block(&block_101b, 101, &hash_101b).unwrap();
	assert_eq!(cs.tip(), hash_101b);
}

#[test]
fn missing_input_rejected() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase only
	let block_1 = make_block(Hash256::ZERO, vec![make_coinbase(1, 50 * COIN)]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	// Try to spend a nonexistent outpoint
	let fake_outpoint = OutPoint {
		txid: Hash256::from_bytes([0xDE; 32]),
		index: 0,
	};
	let spend = make_spend_tx(fake_outpoint, 10 * COIN);
	let block_2 = make_block(hash_1, vec![make_coinbase(2, 50 * COIN), spend]);
	let hash_2 = block_hash(&block_2);

	let result = cs.connect_block(&block_2, 2, &hash_2);
	assert!(result.is_err());
	assert!(
		result.unwrap_err().to_string().contains("missing utxo"),
		"expected missing utxo error"
	);
}

#[test]
fn output_exceeds_input_rejected() {
	let (_dir, cs) = open_chainstate();

	// Block 1: coinbase
	let coinbase = make_coinbase(1, 50 * COIN);
	let coinbase_txid = coinbase.txid();
	let block_1 = make_block(Hash256::ZERO, vec![coinbase]);
	let hash_1 = block_hash(&block_1);
	cs.connect_block(&block_1, 1, &hash_1).unwrap();

	let prev = mature_coinbase(&cs, hash_1, 2, 100);

	// Try to spend 50 COIN and output 51 COIN (more than input)
	let spend = make_spend_tx(
		OutPoint {
			txid: coinbase_txid,
			index: 0,
		},
		51 * COIN,
	);
	let block_101 = make_block(prev, vec![make_coinbase(101, 50 * COIN), spend]);
	let hash_101 = block_hash(&block_101);

	let result = cs.connect_block(&block_101, 101, &hash_101);
	assert!(result.is_err());
	assert!(
		result.unwrap_err().to_string().contains("exceed"),
		"expected outputs exceed inputs error"
	);
}
