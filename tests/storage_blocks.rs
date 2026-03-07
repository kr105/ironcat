// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use ironcat::{storage::block_store::BlockStore, types::hash::Hash256};

#[test]
fn store_and_check_block() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();
	let hash = Hash256::from_bytes([0xAA; 32]);
	let data = vec![1u8; 256];

	assert!(!store.has_block(&hash));
	store.store_block(&hash, &data).unwrap();
	store.flush().unwrap();
	assert!(store.has_block(&hash));
}

#[test]
fn flat_file_rotation_at_limit() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open_with_max_file_size(dir.path(), 1024).unwrap();
	let hash1 = Hash256::from_bytes([0x01; 32]);
	let hash2 = Hash256::from_bytes([0x02; 32]);
	let data = vec![0u8; 600]; // > half of 1024

	store.store_block(&hash1, &data).unwrap();
	store.store_block(&hash2, &data).unwrap();
	store.flush().unwrap();

	assert!(store.has_block(&hash1));
	assert!(store.has_block(&hash2));
	let loc = store.get_location(&hash2).unwrap();
	assert_eq!(loc.file_number, 1);
}

#[test]
fn store_survives_reopen() {
	let dir = tempfile::tempdir().unwrap();
	let hash = Hash256::from_bytes([0xBB; 32]);
	let data = vec![42u8; 128];

	{
		let store = BlockStore::open(dir.path()).unwrap();
		store.store_block(&hash, &data).unwrap();
		store.flush().unwrap();
	}

	let store = BlockStore::open(dir.path()).unwrap();
	assert!(store.has_block(&hash));
}

#[test]
fn duplicate_store_is_idempotent() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();
	let hash = Hash256::from_bytes([0xCC; 32]);
	let data = vec![7u8; 64];

	store.store_block(&hash, &data).unwrap();
	store.flush().unwrap();
	store.store_block(&hash, &data).unwrap(); // should not error (dedup via redb)
	assert!(store.has_block(&hash));
}

#[test]
fn get_location_returns_correct_offset() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();
	let hash = Hash256::from_bytes([0xDD; 32]);
	let data = vec![5u8; 100];

	store.store_block(&hash, &data).unwrap();
	store.flush().unwrap();
	let loc = store.get_location(&hash).unwrap();

	assert_eq!(loc.file_number, 0);
	assert_eq!(loc.offset, 0);
	assert_eq!(loc.size, 100);
}

#[test]
fn second_block_has_correct_offset() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();

	let hash1 = Hash256::from_bytes([0x01; 32]);
	let hash2 = Hash256::from_bytes([0x02; 32]);
	let data1 = vec![1u8; 100];
	let data2 = vec![2u8; 200];

	store.store_block(&hash1, &data1).unwrap();
	store.store_block(&hash2, &data2).unwrap();
	store.flush().unwrap();

	let loc1 = store.get_location(&hash1).unwrap();
	assert_eq!(loc1.offset, 0);
	assert_eq!(loc1.size, 100);

	let loc2 = store.get_location(&hash2).unwrap();
	// offset = magic(4) + size(4) + data(100) = 108
	assert_eq!(loc2.offset, 108);
	assert_eq!(loc2.size, 200);
}

#[test]
fn flat_file_contains_correct_bytes() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();
	let hash = Hash256::from_bytes([0xEE; 32]);
	let data = vec![0xAB; 16];

	store.store_block(&hash, &data).unwrap();

	// Read the flat file and verify format
	let file_path = dir.path().join("blocks/blk00000.dat");
	let contents = std::fs::read(file_path).unwrap();

	// magic(4) + size(4) + data(16) = 24
	assert_eq!(contents.len(), 24);

	// Check magic
	assert_eq!(&contents[0..4], &[0xFC, 0xC1, 0xB7, 0xDC]);

	// Check size (16 as u32 LE)
	assert_eq!(&contents[4..8], &16u32.to_le_bytes());

	// Check data
	assert_eq!(&contents[8..24], &[0xAB; 16]);
}

#[test]
fn nonexistent_block_returns_none() {
	let dir = tempfile::tempdir().unwrap();
	let store = BlockStore::open(dir.path()).unwrap();
	let hash = Hash256::from_bytes([0xFF; 32]);

	assert!(!store.has_block(&hash));
	assert!(store.get_location(&hash).is_none());
}

#[test]
fn stale_index_cleared_when_flat_files_deleted() {
	let dir = tempfile::tempdir().unwrap();
	let hash = Hash256::from_bytes([0xAA; 32]);
	let data = vec![1u8; 256];

	// Store a block and flush so the redb index has entries
	{
		let store = BlockStore::open(dir.path()).unwrap();
		store.store_block(&hash, &data).unwrap();
		store.flush().unwrap();
		assert!(store.has_block(&hash));
	}

	// Delete the blocks directory (simulates user deleting flat files)
	std::fs::remove_dir_all(dir.path().join("blocks")).unwrap();

	// Reopen -- should detect stale index and clear it
	let store = BlockStore::open(dir.path()).unwrap();
	assert!(!store.has_block(&hash));

	// Should be able to store new blocks from scratch
	let hash2 = Hash256::from_bytes([0xBB; 32]);
	store.store_block(&hash2, &data).unwrap();
	store.flush().unwrap();
	assert!(store.has_block(&hash2));
	let loc = store.get_location(&hash2).unwrap();
	assert_eq!(loc.file_number, 0);
	assert_eq!(loc.offset, 0);
}

#[test]
fn stale_index_cleared_when_flat_file_truncated() {
	let dir = tempfile::tempdir().unwrap();
	let hash = Hash256::from_bytes([0xAA; 32]);
	let data = vec![1u8; 256];

	// Store a block and flush
	{
		let store = BlockStore::open(dir.path()).unwrap();
		store.store_block(&hash, &data).unwrap();
		store.flush().unwrap();
		assert!(store.has_block(&hash));
	}

	// Delete blocks/ and recreate it empty (simulates user deleting
	// flat files while index.redb survives, then another run creates
	// an empty blk00000.dat before the stale check)
	std::fs::remove_dir_all(dir.path().join("blocks")).unwrap();
	std::fs::create_dir(dir.path().join("blocks")).unwrap();
	std::fs::write(dir.path().join("blocks/blk00000.dat"), b"tiny").unwrap();

	// Reopen -- file exists but is way smaller than the index expects
	let store = BlockStore::open(dir.path()).unwrap();
	assert!(!store.has_block(&hash));
}

#[test]
fn stale_index_cleared_when_rotated_file_missing() {
	let dir = tempfile::tempdir().unwrap();
	let hash1 = Hash256::from_bytes([0x01; 32]);
	let hash2 = Hash256::from_bytes([0x02; 32]);
	let data = vec![0u8; 600];

	// Store blocks that trigger file rotation (file 0 -> file 1)
	{
		let store = BlockStore::open_with_max_file_size(dir.path(), 1024).unwrap();
		store.store_block(&hash1, &data).unwrap();
		store.store_block(&hash2, &data).unwrap();
		store.flush().unwrap();
		let loc = store.get_location(&hash2).unwrap();
		assert_eq!(loc.file_number, 1);
	}

	// Delete only the rotated file, leaving file 0
	std::fs::remove_file(dir.path().join("blocks/blk00001.dat")).unwrap();

	// Reopen -- should detect that blk00001.dat is missing and clear index
	let store = BlockStore::open_with_max_file_size(dir.path(), 1024).unwrap();
	assert!(!store.has_block(&hash1));
	assert!(!store.has_block(&hash2));
}

#[test]
fn reopen_resumes_at_correct_file() {
	let dir = tempfile::tempdir().unwrap();
	let hash1 = Hash256::from_bytes([0x01; 32]);
	let hash2 = Hash256::from_bytes([0x02; 32]);
	let hash3 = Hash256::from_bytes([0x03; 32]);
	let data = vec![0u8; 600];

	{
		let store = BlockStore::open_with_max_file_size(dir.path(), 1024).unwrap();
		store.store_block(&hash1, &data).unwrap();
		store.store_block(&hash2, &data).unwrap(); // triggers rotation to file 1
		store.flush().unwrap();
	}

	// Reopen and store a third block
	let store = BlockStore::open_with_max_file_size(dir.path(), 1024).unwrap();
	store.store_block(&hash3, &data).unwrap();
	store.flush().unwrap();

	let loc3 = store.get_location(&hash3).unwrap();
	// File 1 already has 608 bytes (8 prefix + 600 data), adding another 608
	// exceeds the 1024 limit, so it rotates to file 2
	assert_eq!(loc3.file_number, 2);
}
