// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use ironcat::{
	difficulty::ConsensusParams,
	headers::HeaderStore,
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
///
/// Each header is unique because the "nonce" field varies via the nonce param
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

/// Helper: compute genesis hash from the test genesis header
fn genesis_hash() -> Hash256 {
	GENESIS_HEADER.block_hash()
}

#[test]
fn catcoin_genesis_hash_matches_reference() {
	// Catcoin mainnet genesis from chainparams.cpp:
	// CreateGenesisBlock(1387838302, 588050, 0x1e0ffff0, 1, 50 * COIN)
	// Hash: bc3b4ec43c4ebb2fef49e6240812549e61ffa623d9418608aa90eaad26c96296
	let genesis = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::from_bytes([
			0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb, 0x13, 0x8d, 0x1c, 0x43,
			0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5, 0x3d, 0xa3, 0x07, 0x40,
		]),
		timestamp: 1_387_838_302,
		bits: 0x1e0f_fff0,
		nonce: 588_050,
	};
	let hash = genesis.block_hash();
	// Wire order: first byte is 0x96
	assert_eq!(hash.as_bytes()[0], 0x96);
	// Display order matches chainparams.cpp assertion
	let display = format!("{hash}");
	assert_eq!(
		display,
		"bc3b4ec43c4ebb2fef49e6240812549e61ffa623d9418608aa90eaad26c96296"
	);
}

#[test]
fn new_store_has_genesis_as_tip() {
	let store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let (tip_hash, tip_height) = store.tip();
	assert_eq!(tip_hash, genesis_hash());
	assert_eq!(tip_height, 0);
}

#[test]
fn new_store_height_is_zero() {
	let store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	assert_eq!(store.height(), 0);
}

#[test]
fn new_store_genesis_returns_genesis_hash() {
	let store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	assert_eq!(store.genesis(), genesis_hash());
}

#[test]
fn add_header_extends_tip() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let header = make_header(genesis_hash(), 1);
	let height = store.add_header(&header).unwrap();
	assert_eq!(height, 1);
	assert_eq!(store.height(), 1);

	let (tip_hash, tip_height) = store.tip();
	assert_eq!(tip_hash, header.block_hash());
	assert_eq!(tip_height, 1);
}

#[test]
fn add_header_rejects_orphan() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let bogus_prev = Hash256::from_bytes([0xAA; 32]);
	let header = make_header(bogus_prev, 1);
	let result = store.add_header(&header);
	assert!(result.is_err());
}

#[test]
fn add_header_rejects_fork_off_non_tip() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());

	let h1 = make_header(genesis_hash(), 1);
	let h1_hash = h1.block_hash();
	store.add_header(&h1).unwrap();

	let h2 = make_header(h1_hash, 2);
	store.add_header(&h2).unwrap();

	// Try to add a header that forks off h1 (not the tip h2)
	let fork = make_header(h1_hash, 999);
	let result = store.add_header(&fork);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(msg.contains("not tip"), "expected fork rejection, got: {msg}");
}

#[test]
fn batch_rejects_broken_continuity() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());

	let h1 = make_header(genesis_hash(), 1);
	// h2 should chain to h1 but instead chains to something else
	let bogus_prev = Hash256::from_bytes([0xBB; 32]);
	let h2 = make_header(bogus_prev, 2);

	let result = store.add_headers(&[h1, h2]);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(
		msg.contains("does not chain"),
		"expected chain continuity error, got: {msg}"
	);
}

#[test]
fn add_header_rejects_duplicate() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let header = make_header(genesis_hash(), 1);
	store.add_header(&header).unwrap();
	let result = store.add_header(&header);
	assert!(result.is_err());
}

#[test]
fn get_returns_stored_header() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let header = make_header(genesis_hash(), 1);
	let hash = header.block_hash();
	store.add_header(&header).unwrap();

	let stored = store.get(&hash).unwrap();
	assert_eq!(stored.height, 1);
	assert_eq!(stored.header, header);
}

#[test]
fn get_returns_none_for_unknown_hash() {
	let store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let unknown = Hash256::from_bytes([0xFF; 32]);
	assert!(store.get(&unknown).is_none());
}

#[test]
fn hash_at_height_returns_correct_hash() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let h1 = make_header(genesis_hash(), 1);
	let h1_hash = h1.block_hash();
	store.add_header(&h1).unwrap();

	assert_eq!(store.hash_at_height(0), Some(genesis_hash()));
	assert_eq!(store.hash_at_height(1), Some(h1_hash));
	assert_eq!(store.hash_at_height(2), None);
}

#[test]
fn chain_of_three_headers() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());

	let h1 = make_header(genesis_hash(), 1);
	let h1_hash = h1.block_hash();
	assert_eq!(store.add_header(&h1).unwrap(), 1);

	let h2 = make_header(h1_hash, 2);
	let h2_hash = h2.block_hash();
	assert_eq!(store.add_header(&h2).unwrap(), 2);

	let h3 = make_header(h2_hash, 3);
	let h3_hash = h3.block_hash();
	assert_eq!(store.add_header(&h3).unwrap(), 3);

	assert_eq!(store.height(), 3);
	assert_eq!(store.tip(), (h3_hash, 3));
	assert_eq!(store.hash_at_height(0), Some(genesis_hash()));
	assert_eq!(store.hash_at_height(1), Some(h1_hash));
	assert_eq!(store.hash_at_height(2), Some(h2_hash));
	assert_eq!(store.hash_at_height(3), Some(h3_hash));
}

#[test]
fn batch_add_headers_works() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());

	let h1 = make_header(genesis_hash(), 10);
	let h1_hash = h1.block_hash();
	let h2 = make_header(h1_hash, 20);
	let h2_hash = h2.block_hash();
	let h3 = make_header(h2_hash, 30);

	let count = store.add_headers(&[h1, h2, h3]).unwrap();
	assert_eq!(count, 3);
	assert_eq!(store.height(), 3);
}

#[test]
fn batch_add_headers_skips_duplicates() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());

	let h1 = make_header(genesis_hash(), 10);
	let h1_hash = h1.block_hash();
	store.add_header(&h1).unwrap();

	let h2 = make_header(h1_hash, 20);
	let h2_hash = h2.block_hash();
	let h3 = make_header(h2_hash, 30);

	// h1 is already in the store, should be skipped without error
	let count = store.add_headers(&[h1, h2, h3]).unwrap();
	assert_eq!(count, 2);
	assert_eq!(store.height(), 3);
}

/// Builds a chain of `n` headers on top of genesis, returning the store
fn build_chain(n: u32) -> HeaderStore {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let mut prev = genesis_hash();
	for i in 0..n {
		let h = make_header(prev, i);
		prev = h.block_hash();
		store.add_header(&h).unwrap();
	}
	store
}

// --- build_locator tests ---

#[test]
fn locator_genesis_only() {
	let store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let locator = store.build_locator();
	assert_eq!(locator.len(), 1);
	assert_eq!(locator[0], genesis_hash());
}

#[test]
fn locator_short_chain() {
	// 5 headers above genesis => heights 0..=5, so 6 entries all consecutive
	let store = build_chain(5);
	let locator = store.build_locator();
	assert_eq!(locator.len(), 6);
	assert_eq!(locator[0], store.hash_at_height(5).unwrap());
	assert_eq!(locator[5], genesis_hash());
	// every entry should be 1 lower than the previous
	for i in 0..5 {
		let h_a = store.get(&locator[i]).unwrap().height;
		let h_b = store.get(&locator[i + 1]).unwrap().height;
		assert_eq!(h_a, h_b + 1);
	}
}

#[test]
fn locator_exponential_stepping() {
	let store = build_chain(50);
	let locator = store.build_locator();
	// After 10 consecutive entries the step doubles, so total should be well under 50 but more than 10
	assert!(locator.len() > 10, "expected >10 entries, got {}", locator.len());
	assert!(locator.len() <= 20, "expected <=20 entries, got {}", locator.len());
	assert_eq!(locator[0], store.hash_at_height(50).unwrap());
	assert_eq!(*locator.last().unwrap(), genesis_hash());
}

#[test]
fn locator_capped_at_101() {
	let store = build_chain(200);
	let locator = store.build_locator();
	assert!(locator.len() <= 101, "locator exceeded 101 entries: {}", locator.len());
}

#[test]
fn locator_uses_height_index() {
	let store = build_chain(30);
	let locator = store.build_locator();
	let (tip_hash, tip_height) = store.tip();
	assert_eq!(locator[0], tip_hash);
	assert_eq!(locator[0], store.hash_at_height(tip_height).unwrap());
	assert_eq!(*locator.last().unwrap(), store.hash_at_height(0).unwrap());
}

// --- get_headers_after tests ---

#[test]
fn get_headers_after_genesis() {
	let store = build_chain(5);
	let headers = store.get_headers_after(0, 2000, Hash256::ZERO);
	assert_eq!(headers.len(), 5);
}

#[test]
fn get_headers_after_respects_max() {
	let store = build_chain(10);
	let headers = store.get_headers_after(0, 3, Hash256::ZERO);
	assert_eq!(headers.len(), 3);
}

#[test]
fn get_headers_after_with_stop_hash() {
	let store = build_chain(10);
	// stop at height 3
	let stop = store.hash_at_height(3).unwrap();
	let headers = store.get_headers_after(0, 2000, stop);
	assert_eq!(headers.len(), 3);
	// last header in result should be the one at height 3
	let last_hash = headers.last().unwrap().block_hash();
	assert_eq!(last_hash, stop);
}

#[test]
fn get_headers_after_at_tip_returns_empty() {
	let store = build_chain(5);
	let tip_height = store.height();
	let headers = store.get_headers_after(tip_height, 2000, Hash256::ZERO);
	assert!(headers.is_empty());
}

// --- difficulty validation tests ---

#[test]
fn add_header_rejects_wrong_difficulty() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	// Header with wrong bits (should be 0x1e0ffff0 at height 1)
	let bad = BlockHeader {
		version: 1,
		prev_hash: genesis_hash(),
		merkle_root: Hash256::ZERO,
		timestamp: 1_400_000_000,
		bits: 0x1c0f_fff0, // wrong difficulty
		nonce: 1,
	};
	let result = store.add_header(&bad);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(
		msg.contains("invalid difficulty"),
		"expected difficulty error, got: {msg}"
	);
}

#[test]
fn add_header_accepts_correct_difficulty() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	// bits = 0x1e0ffff0 matches genesis (no retarget until block 2016)
	let good = make_header(genesis_hash(), 1);
	assert!(store.add_header(&good).is_ok());
}

#[test]
fn batch_rejects_wrong_difficulty() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let h1 = make_header(genesis_hash(), 1);
	let h2 = BlockHeader {
		version: 1,
		prev_hash: h1.block_hash(),
		merkle_root: Hash256::ZERO,
		timestamp: 1_400_000_000,
		bits: 0x1c0f_fff0, // wrong difficulty
		nonce: 2,
	};
	let result = store.add_headers(&[h1, h2]);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(
		msg.contains("invalid difficulty"),
		"expected difficulty error, got: {msg}"
	);
	// First header should NOT have been committed since batch is all-or-nothing
	assert_eq!(store.height(), 0);
}

// --- proof-of-work validation tests ---

/// Real Catcoin mainnet genesis block header (valid scrypt `PoW`)
const REAL_GENESIS: BlockHeader = BlockHeader {
	version: 1,
	prev_hash: Hash256::ZERO,
	merkle_root: Hash256::from_bytes([
		0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb,
		0x13, 0x8d, 0x1c, 0x43, 0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5,
		0x3d, 0xa3, 0x07, 0x40,
	]),
	timestamp: 1_387_838_302,
	bits: 0x1e0f_fff0,
	nonce: 588_050,
};

#[test]
fn pow_rejects_fake_header_without_checkpoint() {
	// Use mainnet params (no checkpoints), so PoW is enforced
	let mut store = HeaderStore::new(REAL_GENESIS, ConsensusParams::mainnet());
	// Fake header with correct difficulty but invalid scrypt hash
	let fake = BlockHeader {
		version: 1,
		prev_hash: REAL_GENESIS.block_hash(),
		merkle_root: Hash256::ZERO,
		timestamp: 1_400_000_000,
		bits: 0x1e0f_fff0,
		nonce: 1, // not a valid PoW nonce
	};
	let result = store.add_header(&fake);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(
		msg.contains("proof-of-work"),
		"expected PoW failure, got: {msg}"
	);
}

#[test]
fn pow_skipped_below_checkpoint() {
	// Checkpoint at height 1000 -- all headers below skip PoW
	let params = ConsensusParams {
		checkpoints: &[(1000, [0u8; 32])],
		..ConsensusParams::mainnet()
	};
	let mut store = HeaderStore::new(GENESIS_HEADER, params);
	// Fake header with no valid PoW -- should be accepted because height 1 < 1000
	let fake = make_header(genesis_hash(), 1);
	assert!(store.add_header(&fake).is_ok());
}

#[test]
fn checkpoint_hash_mismatch_rejected() {
	// Checkpoint at height 1 with a specific hash
	let params = ConsensusParams {
		checkpoints: &[(1, [0xAA; 32])],
		..ConsensusParams::mainnet()
	};
	let mut store = HeaderStore::new(GENESIS_HEADER, params);
	// Header at checkpoint height 1 but with wrong hash
	let header = make_header(genesis_hash(), 1);
	let result = store.add_header(&header);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(
		msg.contains("checkpoint mismatch"),
		"expected checkpoint error, got: {msg}"
	);
}
