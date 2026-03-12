// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing/arithmetic for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::cast_possible_wrap,
	clippy::arithmetic_side_effects
)]

use ironcat::chainstate::coin::Coin;
use ironcat::chainstate::undo::{BlockUndo, TxUndo};
use ironcat::chainstate::undo_store::{UNDO_INDEX, UndoStore};
use ironcat::types::hash::Hash256;
use ironcat::types::transaction::TxOut;
use redb::Database;

fn create_db(datadir: &std::path::Path) -> Database {
	let db_path = datadir.join("chainstate.redb");
	let db = Database::create(&db_path).unwrap();
	// Ensure table exists
	let txn = db.begin_write().unwrap();
	{
		let _t = txn.open_table(UNDO_INDEX).unwrap();
	}
	txn.commit().unwrap();
	db
}

fn make_block_undo(num_txs: usize) -> BlockUndo {
	let tx_undos = (0..num_txs)
		.map(|i| TxUndo {
			spent_outputs: vec![Coin {
				tx_out: TxOut {
					value: (i as i64 + 1) * 1_0000_0000,
					script_pubkey: vec![0x76, 0xa9, 0x14],
				},
				height: (i as u32 + 1) * 100,
				is_coinbase: i == 0,
			}],
		})
		.collect();
	BlockUndo { tx_undos }
}

/// Helper: store undo to file and write index entry in a redb transaction
fn store_and_index(store: &UndoStore, db: &Database, hash: &Hash256, undo: &BlockUndo) {
	let loc = store.store_undo_to_file(hash, undo).unwrap();
	let txn = db.begin_write().unwrap();
	{
		let mut table = txn.open_table(UNDO_INDEX).unwrap();
		table
			.insert(&loc.hash, (loc.file_number, loc.offset, loc.size))
			.unwrap();
	}
	txn.commit().unwrap();
}

#[test]
fn store_and_load_undo() {
	let dir = tempfile::tempdir().unwrap();
	let db = create_db(dir.path());
	let store = UndoStore::open(dir.path(), &db).unwrap();
	let hash = Hash256::from_bytes([0xAA; 32]);
	let undo = make_block_undo(3);

	store_and_index(&store, &db, &hash, &undo);

	let loaded = store.load_undo(&db, &hash).unwrap();
	assert_eq!(loaded.tx_undos.len(), 3);
	assert_eq!(loaded.tx_undos[0].spent_outputs[0].tx_out.value, 1_0000_0000);
	assert_eq!(loaded.tx_undos[0].spent_outputs[0].height, 100);
	assert!(loaded.tx_undos[0].spent_outputs[0].is_coinbase);
	assert_eq!(loaded.tx_undos[1].spent_outputs[0].tx_out.value, 2_0000_0000);
	assert_eq!(loaded.tx_undos[1].spent_outputs[0].height, 200);
	assert!(!loaded.tx_undos[1].spent_outputs[0].is_coinbase);
	assert_eq!(loaded.tx_undos[2].spent_outputs[0].tx_out.value, 3_0000_0000);
}

#[test]
fn load_nonexistent_returns_error() {
	let dir = tempfile::tempdir().unwrap();
	let db = create_db(dir.path());
	let store = UndoStore::open(dir.path(), &db).unwrap();
	let hash = Hash256::from_bytes([0xFF; 32]);

	let result = store.load_undo(&db, &hash);
	assert!(result.is_err());
}

#[test]
fn undo_file_rotation() {
	let dir = tempfile::tempdir().unwrap();
	let db = create_db(dir.path());
	// Small max size to trigger rotation
	let store = UndoStore::open_with_max_file_size(dir.path(), &db, 1024).unwrap();

	// Create a large undo with many txs so serialized size exceeds half of 1024
	let large_undo = make_block_undo(20);
	let hash1 = Hash256::from_bytes([0x01; 32]);
	let hash2 = Hash256::from_bytes([0x02; 32]);

	store_and_index(&store, &db, &hash1, &large_undo);
	store_and_index(&store, &db, &hash2, &large_undo);

	// Both should be loadable
	let loaded1 = store.load_undo(&db, &hash1).unwrap();
	let loaded2 = store.load_undo(&db, &hash2).unwrap();
	assert_eq!(loaded1.tx_undos.len(), 20);
	assert_eq!(loaded2.tx_undos.len(), 20);

	// Second undo should be in rev00001.dat
	assert!(dir.path().join("blocks/rev00001.dat").exists());
}

#[test]
fn undo_survives_reopen() {
	let dir = tempfile::tempdir().unwrap();
	let hash = Hash256::from_bytes([0xBB; 32]);
	let undo = make_block_undo(2);

	{
		let db = create_db(dir.path());
		let store = UndoStore::open(dir.path(), &db).unwrap();
		store_and_index(&store, &db, &hash, &undo);
	}

	// Reopen and verify data is still accessible
	let db = Database::create(dir.path().join("chainstate.redb")).unwrap();
	let store = UndoStore::open(dir.path(), &db).unwrap();
	let loaded = store.load_undo(&db, &hash).unwrap();
	assert_eq!(loaded.tx_undos.len(), 2);
	assert_eq!(loaded.tx_undos[0].spent_outputs[0].tx_out.value, 1_0000_0000);
	assert_eq!(loaded.tx_undos[1].spent_outputs[0].tx_out.value, 2_0000_0000);
}
