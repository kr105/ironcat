// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::arithmetic_side_effects,
	clippy::similar_names
)]

use ironcat::{
	difficulty::ConsensusParams,
	headers::{AcceptResult, HeaderStore},
	types::{block::BlockHeader, hash::Hash256},
};

/// Test genesis block header
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

/// Builds a linear chain of `count` headers on top of genesis
fn build_chain(store: &mut HeaderStore, count: u32) -> Vec<(Hash256, BlockHeader)> {
	let mut chain = Vec::new();
	let mut prev = genesis_hash();
	for i in 1..=count {
		let h = make_header(prev, i * 100);
		let hash = h.block_hash();
		store.add_header(&h).unwrap();
		chain.push((hash, h));
		prev = hash;
	}
	chain
}

// -- accept_header tests --

#[test]
fn accept_header_extends_active_chain() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);
	let prev = chain[2].0;

	// Header extending active tip should be accepted directly
	let h4 = make_header(prev, 400);
	let result = store.accept_header(h4).unwrap();
	assert!(matches!(result, AcceptResult::Accepted));
	assert_eq!(store.height(), 4);
}

#[test]
fn accept_header_already_known() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	let result = store.accept_header(chain[0].1.clone()).unwrap();
	assert!(matches!(result, AcceptResult::AlreadyKnown));
}

#[test]
fn accept_header_creates_fork() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	// Fork off block 1 (height 1) with a different nonce
	let fork_header = make_header(chain[0].0, 999);
	let _fork_hash = fork_header.block_hash();
	let result = store.accept_header(fork_header).unwrap();
	// Fork has less work (2 blocks vs 3), so just Accepted
	assert!(matches!(result, AcceptResult::Accepted));
}

#[test]
fn accept_header_fork_with_more_work_triggers_need_reorg() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let _chain = build_chain(&mut store, 2);

	// Fork off genesis with 3 blocks (more work than 2-block active chain)
	let f1 = make_header(genesis_hash(), 1001);
	let f1_hash = f1.block_hash();
	store.accept_header(f1).unwrap();
	let f2 = make_header(f1_hash, 1002);
	let f2_hash = f2.block_hash();
	store.accept_header(f2).unwrap();
	let f3 = make_header(f2_hash, 1003);
	let result = store.accept_header(f3).unwrap();
	assert!(matches!(result, AcceptResult::NeedReorg));
}

#[test]
fn accept_header_rejects_orphan() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	build_chain(&mut store, 2);

	let orphan = make_header(Hash256::from_bytes([0xAB; 32]), 999);
	assert!(store.accept_header(orphan).is_err());
}

// -- find_fork_point tests --

#[test]
fn find_fork_point_same_hash() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	let fp = store.find_fork_point(chain[2].0, chain[2].0).unwrap();
	assert_eq!(fp, chain[2].0);
}

#[test]
fn find_fork_point_two_branches_from_same_parent() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	// Fork off block 1 (chain[0])
	let fork = make_header(chain[0].0, 777);
	let fork_hash = fork.block_hash();
	store.accept_header(fork).unwrap();

	let fp = store.find_fork_point(chain[2].0, fork_hash).unwrap();
	assert_eq!(fp, chain[0].0);
}

#[test]
fn find_fork_point_different_depths() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 5);

	// Fork at genesis
	let fork = make_header(genesis_hash(), 888);
	let fork_hash = fork.block_hash();
	store.accept_header(fork).unwrap();

	let fp = store.find_fork_point(chain[4].0, fork_hash).unwrap();
	assert_eq!(fp, genesis_hash());
}

// -- get_path tests --

#[test]
fn get_path_returns_hashes_from_fork_to_tip() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 5);

	let path = store.get_path(chain[1].0, chain[4].0).unwrap();
	assert_eq!(path.len(), 3);
	assert_eq!(path[0], (chain[2].0, 3));
	assert_eq!(path[1], (chain[3].0, 4));
	assert_eq!(path[2], (chain[4].0, 5));
}

#[test]
fn get_path_empty_when_same() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	let path = store.get_path(chain[2].0, chain[2].0).unwrap();
	assert!(path.is_empty());
}

// -- switch_active_chain tests --

#[test]
fn switch_active_chain_updates_active_chain() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let _chain = build_chain(&mut store, 3);

	// Create a fork off genesis with 4 blocks
	let f1 = make_header(genesis_hash(), 2001);
	let f1_hash = f1.block_hash();
	store.accept_header(f1).unwrap();
	let f2 = make_header(f1_hash, 2002);
	let f2_hash = f2.block_hash();
	store.accept_header(f2).unwrap();
	let f3 = make_header(f2_hash, 2003);
	let f3_hash = f3.block_hash();
	store.accept_header(f3).unwrap();
	let f4 = make_header(f3_hash, 2004);
	let f4_hash = f4.block_hash();
	store.accept_header(f4).unwrap();

	// Switch to the fork tip
	store.switch_active_chain(f4_hash).unwrap();

	// Active chain should now be: genesis, f1, f2, f3, f4
	assert_eq!(store.height(), 4);
	assert_eq!(store.tip().0, f4_hash);
	assert_eq!(store.hash_at_height(1).unwrap(), f1_hash);
	assert_eq!(store.hash_at_height(2).unwrap(), f2_hash);

	// Old tip should now be in tips set (as a fork tip)
	let (best_hash, _, _) = store.best_tip();
	// f4 has more work, so it should be the best
	assert_eq!(best_hash, f4_hash);
}

// -- mark_invalid tests --

#[test]
fn mark_invalid_removes_from_tips() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	// Create a fork
	let fork = make_header(chain[0].0, 3001);
	let fork_hash = fork.block_hash();
	store.accept_header(fork).unwrap();

	// Mark the fork as invalid
	store.mark_invalid(fork_hash);

	// best_tip should not be the invalid one
	let (best_hash, _, _) = store.best_tip();
	assert_ne!(best_hash, fork_hash);
}

// -- best_tip tests --

#[test]
fn best_tip_returns_cached_best() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	let (hash, height, chainwork) = store.best_tip();
	assert_eq!(hash, chain[2].0);
	assert_eq!(height, 3);
	assert!(chainwork > ironcat::difficulty::U256::zero());
}

// -- purge_stale_forks tests --

#[test]
fn purge_stale_forks_removes_old_tips() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let _chain = build_chain(&mut store, 3);

	// Fork off genesis with 1 header (height 1)
	let fork = make_header(genesis_hash(), 5001);
	let fork_hash = fork.block_hash();
	store.accept_header(fork).unwrap();

	// Fork tip is at height 1, call purge with height 1002 so it's 1001 behind
	store.purge_stale_forks(1002);

	// Fork tip should have been removed
	assert!(store.get(&fork_hash).is_none());
}

#[test]
fn purge_stale_forks_preserves_active_chain() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	// Fork off genesis with 1 header (height 1)
	let fork = make_header(genesis_hash(), 5001);
	store.accept_header(fork).unwrap();

	// Purge with a height that makes the fork stale
	store.purge_stale_forks(1002);

	// Active chain headers must still be present
	assert!(store.get(&genesis_hash()).is_some());
	for (hash, _) in &chain {
		assert!(store.get(hash).is_some());
	}
	assert_eq!(store.height(), 3);
}

#[test]
fn purge_stale_forks_recomputes_best_tip() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 2);

	// Fork off genesis with 3 blocks (more work, triggers NeedReorg)
	let f1 = make_header(genesis_hash(), 6001);
	let f1_hash = f1.block_hash();
	store.accept_header(f1).unwrap();
	let f2 = make_header(f1_hash, 6002);
	let f2_hash = f2.block_hash();
	store.accept_header(f2).unwrap();
	let f3 = make_header(f2_hash, 6003);
	let _f3_hash = f3.block_hash();
	let result = store.accept_header(f3).unwrap();
	assert!(matches!(result, AcceptResult::NeedReorg));

	// Purge the fork (height 3 vs claimed tip 1004 => 1001 behind)
	store.purge_stale_forks(1004);

	// best_tip should now be the active chain tip, not the purged fork
	let (best_hash, _, _) = store.best_tip();
	assert_eq!(best_hash, chain[1].0);
}

// -- remove_fork shared ancestor tests --

#[test]
fn remove_fork_shared_ancestor() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let _chain = build_chain(&mut store, 3);

	// Create a non-active-chain parent off genesis
	let shared = make_header(genesis_hash(), 7001);
	let shared_hash = shared.block_hash();
	store.accept_header(shared).unwrap();

	// Two forks off the shared parent
	let fork_a = make_header(shared_hash, 7002);
	let fork_a_hash = fork_a.block_hash();
	store.accept_header(fork_a).unwrap();

	let fork_b = make_header(shared_hash, 7003);
	let fork_b_hash = fork_b.block_hash();
	store.accept_header(fork_b).unwrap();

	// Extend fork_b so its tip is at height 3 (not stale at purge height 1003)
	let fork_b2 = make_header(fork_b_hash, 7004);
	let fork_b2_hash = fork_b2.block_hash();
	store.accept_header(fork_b2).unwrap();

	// Purge: fork_a tip is at height 2 (1003 - 2 = 1001 > 1000, stale)
	// fork_b2 tip is at height 3 (1003 - 3 = 1000, NOT stale)
	store.purge_stale_forks(1003);

	// fork_a should be gone
	assert!(store.get(&fork_a_hash).is_none());

	// Shared ancestor must still exist (fork_b depends on it)
	assert!(store.get(&shared_hash).is_some());

	// fork_b chain must still be intact
	assert!(store.get(&fork_b_hash).is_some());
	assert!(store.get(&fork_b2_hash).is_some());
}

// -- find_fork_point bounded convergence --

#[test]
fn find_fork_point_bounded_convergence() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 5);

	// Two forks off chain[2] (height 3)
	let fork_a = make_header(chain[2].0, 8001);
	let fork_a_hash = fork_a.block_hash();
	store.accept_header(fork_a).unwrap();

	let fork_b = make_header(chain[2].0, 8002);
	let fork_b_hash = fork_b.block_hash();
	store.accept_header(fork_b).unwrap();

	// Fork point should be chain[2] (their common parent)
	let fp = store.find_fork_point(fork_a_hash, fork_b_hash).unwrap();
	assert_eq!(fp, chain[2].0);
}

// -- accept_header rejects extending invalid --

#[test]
fn accept_header_rejects_extending_invalid() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 3);

	// Create a fork and mark it invalid
	let fork = make_header(chain[1].0, 9001);
	let fork_hash = fork.block_hash();
	store.accept_header(fork).unwrap();
	store.mark_invalid(fork_hash);

	// Try to extend the invalid tip
	let child = make_header(fork_hash, 9002);
	let result = store.accept_header(child);
	assert!(result.is_err());
}

// -- switch_active_chain updates active set --

#[test]
fn switch_active_chain_updates_active_set() {
	let mut store = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
	let chain = build_chain(&mut store, 4);

	// Fork off chain[1] (height 2) with 4 blocks to have more work
	let f1 = make_header(chain[1].0, 10_001);
	let f1_hash = f1.block_hash();
	store.accept_header(f1).unwrap();
	let f2 = make_header(f1_hash, 10_002);
	let f2_hash = f2.block_hash();
	store.accept_header(f2).unwrap();
	let f3 = make_header(f2_hash, 10_003);
	let f3_hash = f3.block_hash();
	store.accept_header(f3).unwrap();
	let f4 = make_header(f3_hash, 10_004);
	let f4_hash = f4.block_hash();
	store.accept_header(f4).unwrap();

	// Switch to the fork
	store.switch_active_chain(f4_hash).unwrap();

	// Old chain headers above the fork point (height 2) should NOT be in active chain
	// chain[1] is at height 2 (fork point), chain[2] is height 3, chain[3] is height 4
	// hash_at_height(3) should now return f1_hash, not chain[2]
	assert_eq!(store.hash_at_height(3).unwrap(), f1_hash);
	assert_eq!(store.hash_at_height(4).unwrap(), f2_hash);
	assert_eq!(store.hash_at_height(5).unwrap(), f3_hash);
	assert_eq!(store.hash_at_height(6).unwrap(), f4_hash);

	// Old active chain blocks should NOT appear at their old heights
	assert_ne!(store.hash_at_height(3).unwrap(), chain[2].0);
	assert_ne!(store.hash_at_height(4).unwrap(), chain[3].0);
}
