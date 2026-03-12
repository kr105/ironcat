// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::items_after_statements,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]

use ironcat::storage::{checksummed_decode, checksummed_encode, open_or_recreate_db};

#[test]
fn roundtrip_encode_decode() {
	let data: Vec<u8> = vec![1, 2, 3, 4, 5];
	let encoded = checksummed_encode(&data).unwrap();
	let decoded: Vec<u8> = checksummed_decode(&encoded).unwrap();
	assert_eq!(data, decoded);
}

#[test]
fn corrupt_checksum_rejected() {
	let data: Vec<u8> = vec![1, 2, 3];
	let mut encoded = checksummed_encode(&data).unwrap();
	encoded[0] ^= 0xFF;
	let result: Result<Vec<u8>, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn corrupt_payload_rejected() {
	let data: Vec<u8> = vec![1, 2, 3];
	let mut encoded = checksummed_encode(&data).unwrap();
	let last = encoded.len() - 1;
	encoded[last] ^= 0xFF;
	let result: Result<Vec<u8>, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn too_short_rejected() {
	let result: Result<Vec<u8>, _> = checksummed_decode(&[0u8; 10]);
	assert!(result.is_err());
}

#[test]
fn empty_payload_roundtrips() {
	let data: Vec<u8> = vec![];
	let encoded = checksummed_encode(&data).unwrap();
	let decoded: Vec<u8> = checksummed_decode(&encoded).unwrap();
	assert_eq!(data, decoded);
}

#[test]
fn large_payload_roundtrip() {
	let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
	let encoded = checksummed_encode(&data).unwrap();
	let decoded: Vec<u8> = checksummed_decode(&encoded).unwrap();
	assert_eq!(data, decoded);
}

#[test]
fn garbage_payload_with_valid_checksum_rejected() {
	use sha2::{Digest, Sha256};

	// Craft a payload that has a valid checksum but nonsense rkyv data
	let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03];
	let checksum = Sha256::digest(&garbage);

	let mut encoded = Vec::with_capacity(32 + garbage.len());
	encoded.extend_from_slice(&checksum);
	encoded.extend_from_slice(&garbage);

	let result: Result<Vec<u8>, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn checksum_length_boundary() {
	// Exactly 32 bytes = valid checksum but empty payload
	let result: Result<Vec<u8>, _> = checksummed_decode(&[0u8; 32]);
	// Should fail because the checksum won't match the empty payload
	// (all-zero checksum != SHA256 of empty)
	assert!(result.is_err());
}

#[test]
fn deterministic_encoding() {
	// Same input should produce the same encoded output
	let data: Vec<u8> = vec![42, 0, 255, 128];
	let encoded1 = checksummed_encode(&data).unwrap();
	let encoded2 = checksummed_encode(&data).unwrap();
	assert_eq!(encoded1, encoded2);
}

#[test]
fn open_or_recreate_db_creates_fresh() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("test.redb");
	let db = open_or_recreate_db(&db_path).unwrap();
	// Should be able to begin a transaction on the new db
	let _txn = db.begin_write().unwrap();
}

#[test]
fn open_or_recreate_db_reopens_existing() {
	use redb::{ReadableDatabase, TableDefinition};

	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("test.redb");

	const TABLE: TableDefinition<u32, u32> = TableDefinition::new("test");

	// Create and write data
	{
		let db = open_or_recreate_db(&db_path).unwrap();
		let txn = db.begin_write().unwrap();
		{
			let mut table = txn.open_table(TABLE).unwrap();
			table.insert(1, 42).unwrap();
		}
		txn.commit().unwrap();
	}

	// Reopen and verify data survived
	{
		let db = open_or_recreate_db(&db_path).unwrap();
		let txn = db.begin_read().unwrap();
		let table = txn.open_table(TABLE).unwrap();
		assert_eq!(table.get(1).unwrap().unwrap().value(), 42);
	}
}

#[test]
fn open_or_recreate_db_recovers_from_corrupt_file() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("test.redb");

	// Write random garbage -- should trigger an I/O or corruption error
	// and get automatically deleted + recreated
	std::fs::write(&db_path, b"not_a_real_database_at_all_just_garbage_bytes_here").unwrap();

	let db = open_or_recreate_db(&db_path).unwrap();
	// Should have a working fresh database
	let _txn = db.begin_write().unwrap();
}

#[test]
fn open_or_recreate_db_recovers_from_truncated_db() {
	use redb::TableDefinition;

	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("test.redb");

	const TABLE: TableDefinition<u32, u32> = TableDefinition::new("test");

	// Create a valid database with some data
	{
		let db = open_or_recreate_db(&db_path).unwrap();
		let txn = db.begin_write().unwrap();
		{
			let mut table = txn.open_table(TABLE).unwrap();
			for i in 0..100 {
				table.insert(i, i * 10).unwrap();
			}
		}
		txn.commit().unwrap();
	}

	// Truncate the file to corrupt it
	let file = std::fs::OpenOptions::new().write(true).open(&db_path).unwrap();
	file.set_len(64).unwrap();
	drop(file);

	// Should detect corruption and recreate
	let db = open_or_recreate_db(&db_path).unwrap();
	let _txn = db.begin_write().unwrap();
}
