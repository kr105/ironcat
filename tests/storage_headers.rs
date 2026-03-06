// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use std::sync::{
	atomic::{AtomicBool, Ordering},
	Arc,
};

use anyhow::{bail, Result};
use ironcat::{
	headers::HeaderStore,
	storage::{header_store_backend::HeaderStoreBackend, header_store_redb::RedbHeaderStore},
	types::{block::BlockHeader, hash::Hash256},
};

/// Test genesis block header (not real Catcoin genesis, just for test isolation)
const GENESIS_HEADER: BlockHeader = BlockHeader {
	version: 1,
	prev_hash: Hash256::ZERO,
	merkle_root: Hash256::ZERO,
	timestamp: 1_387_838_302,
	bits: 0x1e0f_fff0,
	nonce: 42,
};

/// Builds a fake block header whose `prev_hash` is the given hash
const fn make_header(prev_hash: Hash256, nonce: u32) -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash,
		merkle_root: Hash256::ZERO,
		timestamp: 1_400_000_000,
		bits: 0x1e0f_fff0,
		nonce,
	}
}

fn genesis_hash() -> Hash256 {
	GENESIS_HEADER.block_hash()
}

/// Helper: open a `RedbHeaderStore` at a path, returning a trait object
fn open_at(path: &std::path::Path) -> Arc<dyn HeaderStoreBackend> {
	Arc::new(RedbHeaderStore::open(path).unwrap())
}

// -- Happy path tests --

#[test]
fn genesis_persisted_on_first_open() {
	let dir = tempfile::tempdir().unwrap();
	let backend = open_at(&dir.path().join("headers.redb"));

	let _store = HeaderStore::with_backend(GENESIS_HEADER, Some(Arc::clone(&backend)));

	assert_eq!(backend.count().unwrap(), 1);
}

#[test]
fn headers_survive_reopen() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("headers.redb");

	let h1 = make_header(genesis_hash(), 100);
	let h1_hash = h1.block_hash();
	let h2 = make_header(h1_hash, 200);
	let h2_hash = h2.block_hash();
	let h3 = make_header(h2_hash, 300);
	let h3_hash = h3.block_hash();

	// First session: add 3 headers
	{
		let backend = open_at(&db_path);
		let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
		store.add_header(&h1).unwrap();
		store.add_header(&h2).unwrap();
		store.add_header(&h3).unwrap();
		assert_eq!(store.height(), 3);
	}

	// Second session: reopen and verify
	{
		let backend = open_at(&db_path);
		let store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
		assert_eq!(store.height(), 3);

		let (tip_hash, tip_height) = store.tip();
		assert_eq!(tip_height, 3);
		assert_eq!(tip_hash, h3_hash);

		assert_eq!(store.hash_at_height(1).unwrap(), h1_hash);
		assert_eq!(store.hash_at_height(2).unwrap(), h2_hash);
		assert_eq!(store.hash_at_height(3).unwrap(), h3_hash);
	}
}

#[test]
fn batch_add_persists_all() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("headers.redb");

	let h1 = make_header(genesis_hash(), 100);
	let h2 = make_header(h1.block_hash(), 200);
	let h3 = make_header(h2.block_hash(), 300);

	// First session: batch add
	{
		let backend = open_at(&db_path);
		let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(Arc::clone(&backend)));
		let added = store.add_headers(&[h1, h2, h3]).unwrap();
		assert_eq!(added, 3);
		assert_eq!(backend.count().unwrap(), 4); // genesis + 3
	}

	// Second session: verify all survived
	{
		let backend = open_at(&db_path);
		let store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
		assert_eq!(store.height(), 3);
	}
}

#[test]
fn genesis_verified_on_reopen() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("headers.redb");

	// First session: create with test genesis
	{
		let backend = open_at(&db_path);
		let _store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
	}

	// Second session: reopen with a DIFFERENT genesis -- should clear and start fresh
	let different_genesis = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 9_999_999,
		bits: 0x1e0f_fff0,
		nonce: 999,
	};

	{
		let backend = open_at(&db_path);
		let store = HeaderStore::with_backend(different_genesis.clone(), Some(Arc::clone(&backend)));
		assert_eq!(store.height(), 0);
		assert_eq!(store.genesis(), different_genesis.block_hash());
		// Database should have been cleared and new genesis persisted
		assert_eq!(backend.count().unwrap(), 1);
	}
}

#[test]
fn redb_backend_count_accurate() {
	let dir = tempfile::tempdir().unwrap();
	let backend = open_at(&dir.path().join("headers.redb"));
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(Arc::clone(&backend)));

	assert_eq!(backend.count().unwrap(), 1);

	let h1 = make_header(genesis_hash(), 100);
	store.add_header(&h1).unwrap();
	assert_eq!(backend.count().unwrap(), 2);

	let h2 = make_header(h1.block_hash(), 200);
	store.add_header(&h2).unwrap();
	assert_eq!(backend.count().unwrap(), 3);
}

#[test]
fn load_all_returns_sorted_by_height() {
	let dir = tempfile::tempdir().unwrap();
	let backend = open_at(&dir.path().join("headers.redb"));
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(Arc::clone(&backend)));

	let h1 = make_header(genesis_hash(), 100);
	let h2 = make_header(h1.block_hash(), 200);
	let h3 = make_header(h2.block_hash(), 300);

	store.add_header(&h1).unwrap();
	store.add_header(&h2).unwrap();
	store.add_header(&h3).unwrap();

	let all = backend.load_all().unwrap();
	assert_eq!(all.len(), 4);

	for (i, (_header, height)) in all.iter().enumerate() {
		assert_eq!(*height, i as u32);
	}
}

#[test]
fn locator_works_after_reload() {
	let dir = tempfile::tempdir().unwrap();
	let db_path = dir.path().join("headers.redb");

	let mut headers = Vec::new();
	let mut prev = genesis_hash();
	for i in 1..=20u32 {
		let h = make_header(prev, i);
		prev = h.block_hash();
		headers.push(h);
	}

	let locator_before;

	{
		let backend = open_at(&db_path);
		let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
		store.add_headers(&headers).unwrap();
		locator_before = store.build_locator();
	}

	{
		let backend = open_at(&db_path);
		let store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
		let locator_after = store.build_locator();
		assert_eq!(locator_before, locator_after);
	}
}

// -- Failure path tests --

/// Mock backend that can be configured to fail on persist operations
struct FailingBackend {
	should_fail: AtomicBool,
}

impl FailingBackend {
	const fn new(should_fail: bool) -> Self {
		Self {
			should_fail: AtomicBool::new(should_fail),
		}
	}
}

impl HeaderStoreBackend for FailingBackend {
	fn persist_header(&self, _header: &BlockHeader, _height: u32) -> Result<()> {
		if self.should_fail.load(Ordering::Relaxed) {
			bail!("simulated persist failure");
		}
		Ok(())
	}

	fn persist_headers(&self, _headers: &[(BlockHeader, u32)]) -> Result<()> {
		if self.should_fail.load(Ordering::Relaxed) {
			bail!("simulated batch persist failure");
		}
		Ok(())
	}

	fn load_all(&self) -> Result<Vec<(BlockHeader, u32)>> {
		Ok(Vec::new())
	}

	fn count(&self) -> Result<u64> {
		Ok(0)
	}

	fn clear(&self) -> Result<()> {
		Ok(())
	}
}

#[test]
fn genesis_persist_failure_disables_backend() {
	let backend: Arc<dyn HeaderStoreBackend> = Arc::new(FailingBackend::new(true));
	// Genesis persist will fail, backend should be disabled, store still works in-memory
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend));
	assert_eq!(store.height(), 0);

	// Since backend was disabled after genesis persist failure,
	// add_header should succeed (no backend to fail)
	let h1 = make_header(genesis_hash(), 100);
	assert!(store.add_header(&h1).is_ok());
}

#[test]
fn persist_failure_on_add_header_blocks_insert() {
	let backend = Arc::new(FailingBackend::new(false));
	let backend_trait: Arc<dyn HeaderStoreBackend> = Arc::clone(&backend) as Arc<dyn HeaderStoreBackend>;
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend_trait));

	// First header succeeds
	let h1 = make_header(genesis_hash(), 100);
	assert!(store.add_header(&h1).is_ok());
	assert_eq!(store.height(), 1);

	// Now make persist fail
	backend.should_fail.store(true, Ordering::Relaxed);

	let h2 = make_header(h1.block_hash(), 200);
	assert!(store.add_header(&h2).is_err());
	// In-memory state should NOT have advanced
	assert_eq!(store.height(), 1);
}

#[test]
fn batch_persist_failure_blocks_all_inserts() {
	let backend = Arc::new(FailingBackend::new(false));
	let backend_trait: Arc<dyn HeaderStoreBackend> = Arc::clone(&backend) as Arc<dyn HeaderStoreBackend>;
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(backend_trait));

	// Make persist fail
	backend.should_fail.store(true, Ordering::Relaxed);

	let h1 = make_header(genesis_hash(), 100);
	let h2 = make_header(h1.block_hash(), 200);

	assert!(store.add_headers(&[h1, h2]).is_err());
	// In-memory state should NOT have advanced
	assert_eq!(store.height(), 0);
}

#[test]
fn redb_clear_wipes_all_data() {
	let dir = tempfile::tempdir().unwrap();
	let backend = open_at(&dir.path().join("headers.redb"));
	let mut store = HeaderStore::with_backend(GENESIS_HEADER, Some(Arc::clone(&backend)));

	let h1 = make_header(genesis_hash(), 100);
	store.add_header(&h1).unwrap();
	assert_eq!(backend.count().unwrap(), 2);

	backend.clear().unwrap();
	assert_eq!(backend.count().unwrap(), 0);
}
