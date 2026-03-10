// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::type_complexity,
	clippy::unchecked_time_subtraction
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ironcat::chainstate::ChainState;
use ironcat::difficulty::ConsensusParams;
use ironcat::nodes::block_download::BlockDownloadManager;
use ironcat::nodes::NodeManager;
use ironcat::storage::block_store::BlockStore;
use ironcat::types::block::BlockHeader;
use ironcat::types::hash::Hash256;

/// Dummy genesis header for tests
const fn test_genesis() -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 0,
		bits: 0,
		nonce: 0,
	}
}

fn setup() -> (
	Arc<NodeManager>,
	Arc<BlockStore>,
	tokio::sync::mpsc::Receiver<(Hash256, Vec<u8>)>,
	tokio::sync::mpsc::UnboundedReceiver<std::net::IpAddr>,
	Arc<ChainState>,
	tempfile::TempDir,
) {
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
	let dir = tempfile::tempdir().unwrap();
	let bs = Arc::new(BlockStore::open(dir.path()).unwrap());
	let cs = Arc::new(ChainState::open(dir.path(), u32::MAX).unwrap());
	let (_tx, rx) = tokio::sync::mpsc::channel(32);
	let (_dtx, drx) = tokio::sync::mpsc::unbounded_channel();
	(nm, bs, rx, drx, cs, dir)
}

#[tokio::test]
async fn manager_starts_at_height_one() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);
	// With no headers beyond genesis and no in-flight, it should be caught up
	// chain_height is 0 (genesis only), next_height is 1, so 1 > 0 = true
	assert!(mgr.is_caught_up());
}

#[tokio::test]
async fn timeout_scan_removes_stale_entries() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	// Manually inject an in-flight entry with an old timestamp
	let hash = Hash256::from_bytes([0xAA; 32]);
	let fake_ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();
	let old_time = Instant::now() - Duration::from_secs(120);

	mgr.in_flight.insert(
		hash,
		ironcat::nodes::block_download::InFlightEntry {
			peer: fake_ip,
			requested_at: old_time,
			height: 5,
		},
	);
	mgr.next_height = 100;

	mgr.handle_timeout_scan();

	// Entry should be removed
	assert!(mgr.in_flight.is_empty());
	// next_height should be reset to the stale height
	assert_eq!(mgr.next_height, 5);
}

#[tokio::test]
async fn timeout_scan_resets_to_min_stale_height() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	let fake_ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();
	let old_time = Instant::now() - Duration::from_secs(120);

	// Two stale entries at different heights
	let hash_a = Hash256::from_bytes([0xAA; 32]);
	let hash_b = Hash256::from_bytes([0xBB; 32]);

	mgr.in_flight.insert(
		hash_a,
		ironcat::nodes::block_download::InFlightEntry {
			peer: fake_ip,
			requested_at: old_time,
			height: 10,
		},
	);
	mgr.in_flight.insert(
		hash_b,
		ironcat::nodes::block_download::InFlightEntry {
			peer: fake_ip,
			requested_at: old_time,
			height: 3,
		},
	);
	mgr.next_height = 50;

	mgr.handle_timeout_scan();

	assert!(mgr.in_flight.is_empty());
	// Should reset to the minimum stale height
	assert_eq!(mgr.next_height, 3);
}

#[tokio::test]
async fn timeout_scan_ignores_fresh_entries() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	let fake_ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();
	let hash = Hash256::from_bytes([0xCC; 32]);

	mgr.in_flight.insert(
		hash,
		ironcat::nodes::block_download::InFlightEntry {
			peer: fake_ip,
			requested_at: Instant::now(), // fresh
			height: 7,
		},
	);
	mgr.next_height = 20;

	mgr.handle_timeout_scan();

	// Should not be removed
	assert_eq!(mgr.in_flight.len(), 1);
	assert_eq!(mgr.next_height, 20);
}

#[tokio::test]
async fn is_caught_up_false_with_in_flight() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	let hash = Hash256::from_bytes([0xDD; 32]);
	let fake_ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();

	mgr.in_flight.insert(
		hash,
		ironcat::nodes::block_download::InFlightEntry {
			peer: fake_ip,
			requested_at: Instant::now(),
			height: 1,
		},
	);

	assert!(!mgr.is_caught_up());
}

#[tokio::test]
async fn peer_disconnect_expires_in_flight_blocks() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	let peer_a: std::net::IpAddr = "1.2.3.4".parse().unwrap();
	let peer_b: std::net::IpAddr = "5.6.7.8".parse().unwrap();
	let now = Instant::now();

	// Two blocks from peer_a, one from peer_b
	mgr.in_flight.insert(
		Hash256::from_bytes([0xAA; 32]),
		ironcat::nodes::block_download::InFlightEntry {
			peer: peer_a,
			requested_at: now,
			height: 10,
		},
	);
	mgr.in_flight.insert(
		Hash256::from_bytes([0xBB; 32]),
		ironcat::nodes::block_download::InFlightEntry {
			peer: peer_a,
			requested_at: now,
			height: 5,
		},
	);
	mgr.in_flight.insert(
		Hash256::from_bytes([0xCC; 32]),
		ironcat::nodes::block_download::InFlightEntry {
			peer: peer_b,
			requested_at: now,
			height: 20,
		},
	);
	mgr.next_height = 50;

	mgr.handle_peer_disconnect(peer_a);

	// Only peer_b's entry should remain
	assert_eq!(mgr.in_flight.len(), 1);
	assert!(mgr.in_flight.values().all(|e| e.peer == peer_b));
	// next_height should be reset to min of expired heights (5)
	assert_eq!(mgr.next_height, 5);
}

#[tokio::test]
async fn peer_disconnect_no_in_flight_is_noop() {
	let (nm, bs, rx, drx, cs, _dir) = setup();
	let mut mgr = BlockDownloadManager::new(nm, bs, rx, drx, cs);

	let peer: std::net::IpAddr = "1.2.3.4".parse().unwrap();
	mgr.next_height = 50;

	mgr.handle_peer_disconnect(peer);

	assert!(mgr.in_flight.is_empty());
	assert_eq!(mgr.next_height, 50);
}
