// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::Path;
use std::sync::Arc;

use ironcat::chainstate::ChainState;
use ironcat::mempool::Mempool;
use ironcat::types::block::{Block, BlockHeader};
use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};

/// Creates a temporary chainstate for testing
fn test_chainstate(dir: &Path) -> Arc<ChainState> {
	Arc::new(ChainState::open(dir, 0).unwrap())
}

/// Creates a simple transaction spending the given outpoint
fn make_tx(inputs: Vec<OutPoint>, outputs: Vec<i64>) -> Transaction {
	Transaction {
		version: 1,
		vin: inputs
			.into_iter()
			.map(|prev_output| TxIn {
				prev_output,
				script_sig: vec![0x01, 0x01], // minimal valid scriptsig
				sequence: 0xFFFF_FFFF,
			})
			.collect(),
		vout: outputs
			.into_iter()
			.map(|value| TxOut {
				value,
				script_pubkey: vec![0x51], // OP_TRUE for testing
			})
			.collect(),
		locktime: 0,
	}
}

const fn dummy_block_header() -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 0,
		bits: 0,
		nonce: 0,
	}
}

/// Helper: inserts a UTXO directly into the chainstate database for testing
fn seed_utxo(cs: &ChainState, outpoint: &OutPoint, value: i64) {
	use ironcat::chainstate::UTXO_SET;
	use ironcat::chainstate::coin::Coin;
	use ironcat::types::transaction::TxOut;

	let coin = Coin {
		tx_out: TxOut {
			value,
			script_pubkey: vec![0x51], // OP_TRUE
		},
		height: 1,
		is_coinbase: false,
	};

	let mut key = [0u8; 36];
	key[..32].copy_from_slice(outpoint.txid.as_bytes());
	key[32..].copy_from_slice(&outpoint.index.to_le_bytes());

	let txn = cs.database().begin_write().unwrap();
	{
		let mut table = txn.open_table(UTXO_SET).unwrap();
		table.insert(&key, coin.to_bytes().as_slice()).unwrap();
	}
	txn.commit().unwrap();
}

#[test]
fn accept_rejects_coinbase() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let coinbase = Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig: vec![0x04, 0x00, 0x00, 0x00],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_0000_0000,
			script_pubkey: vec![0x51],
		}],
		locktime: 0,
	};

	let result = mempool.accept_tx(coinbase, &cs);
	assert!(result.is_err(), "coinbase should be rejected");
}

#[test]
fn accept_rejects_duplicate() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([1u8; 32]),
		index: 0,
	};

	seed_utxo(&cs, &outpoint, 100_000);

	let tx = make_tx(vec![outpoint], vec![90_000]);
	mempool.accept_tx(tx.clone(), &cs).unwrap();

	let result = mempool.accept_tx(tx, &cs);
	assert!(result.is_err(), "duplicate tx should be rejected");
}

#[test]
fn accept_rejects_missing_inputs() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let tx = make_tx(
		vec![OutPoint {
			txid: Hash256::from_bytes([99u8; 32]),
			index: 0,
		}],
		vec![50_000],
	);

	let result = mempool.accept_tx(tx, &cs);
	assert!(result.is_err(), "tx with missing inputs should be rejected");
}

#[test]
fn accept_valid_tx() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([2u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 100_000);

	let tx = make_tx(vec![outpoint], vec![90_000]);
	mempool.accept_tx(tx, &cs).unwrap();

	let stats = mempool.get_stats();
	assert_eq!(stats.count, 1);
	assert!(stats.total_size > 0);
}

#[test]
fn accept_rejects_double_spend_in_mempool() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([3u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 100_000);

	let tx1 = make_tx(vec![outpoint.clone()], vec![90_000]);
	mempool.accept_tx(tx1, &cs).unwrap();

	let tx2 = make_tx(vec![outpoint], vec![80_000]);
	let result = mempool.accept_tx(tx2, &cs);
	assert!(result.is_err(), "double-spend should be rejected");
}

#[test]
fn accept_tx_chain_parent_in_mempool() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([4u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 200_000);

	let parent = make_tx(vec![outpoint], vec![190_000]);
	let parent_txid = mempool.accept_tx(parent, &cs).unwrap();

	let child_outpoint = OutPoint {
		txid: parent_txid,
		index: 0,
	};
	let child = make_tx(vec![child_outpoint], vec![180_000]);
	let result = mempool.accept_tx(child, &cs);
	assert!(result.is_ok(), "child spending mempool parent should be accepted");

	let stats = mempool.get_stats();
	assert_eq!(stats.count, 2);
}

#[test]
fn evict_removes_lowest_fee_rate() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::with_max_size(100);

	let outpoint1 = OutPoint {
		txid: Hash256::from_bytes([10u8; 32]),
		index: 0,
	};
	let outpoint2 = OutPoint {
		txid: Hash256::from_bytes([11u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint1, 100_000);
	seed_utxo(&cs, &outpoint2, 200_000);

	let tx1 = make_tx(vec![outpoint1], vec![99_900]);
	let txid1 = mempool.accept_tx(tx1, &cs).unwrap();

	let tx2 = make_tx(vec![outpoint2], vec![190_000]);
	let txid2 = mempool.accept_tx(tx2, &cs).unwrap();

	assert!(!mempool.contains(&txid1), "low fee tx should be evicted");
	assert!(mempool.contains(&txid2), "high fee tx should remain");
}

#[test]
fn evict_cascades_to_descendants() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([20u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 300_000);

	let parent = make_tx(vec![outpoint], vec![290_000]);
	let parent_txid = mempool.accept_tx(parent, &cs).unwrap();

	let child = make_tx(
		vec![OutPoint {
			txid: parent_txid,
			index: 0,
		}],
		vec![280_000],
	);
	let child_txid = mempool.accept_tx(child, &cs).unwrap();

	assert_eq!(mempool.len(), 2);

	mempool.remove_with_descendants(parent_txid);

	assert_eq!(mempool.len(), 0, "child should be removed when parent is removed");
	assert!(!mempool.contains(&parent_txid));
	assert!(!mempool.contains(&child_txid));
}

#[test]
fn remove_block_txs_removes_confirmed() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([30u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 100_000);

	let tx = make_tx(vec![outpoint], vec![90_000]);
	let _txid = mempool.accept_tx(tx.clone(), &cs).unwrap();
	assert_eq!(mempool.len(), 1);

	let block = Block {
		header: dummy_block_header(),
		transactions: vec![tx],
	};

	mempool.remove_block_txs(&block);
	assert_eq!(mempool.len(), 0);
}

#[test]
fn remove_block_txs_cascades_conflicts() {
	let dir = tempfile::tempdir().unwrap();
	let cs = test_chainstate(dir.path());
	let mut mempool = Mempool::new();

	let outpoint = OutPoint {
		txid: Hash256::from_bytes([31u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &outpoint, 100_000);

	let mempool_tx = make_tx(vec![outpoint.clone()], vec![90_000]);
	let mempool_txid = mempool.accept_tx(mempool_tx, &cs).unwrap();

	let child = make_tx(
		vec![OutPoint {
			txid: mempool_txid,
			index: 0,
		}],
		vec![80_000],
	);
	let child_txid = mempool.accept_tx(child, &cs).unwrap();
	assert_eq!(mempool.len(), 2);

	let block_tx = make_tx(vec![outpoint], vec![95_000]);
	let block = Block {
		header: dummy_block_header(),
		transactions: vec![block_tx],
	};

	mempool.remove_block_txs(&block);

	assert_eq!(mempool.len(), 0);
	assert!(!mempool.contains(&mempool_txid));
	assert!(!mempool.contains(&child_txid));
}
