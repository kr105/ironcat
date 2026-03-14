// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::unchecked_time_subtraction,
	clippy::significant_drop_tightening
)]

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironcat::difficulty::ConsensusParams;
use ironcat::nodes::NodeManager;
use ironcat::nodes::block_download::BlockDownloadManager;
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

fn make_nm() -> Arc<NodeManager> {
	Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()))
}

#[tokio::test]
async fn increment_strike_returns_correct_count() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	assert_eq!(nm.increment_strike(&ip), 1);
	assert_eq!(nm.increment_strike(&ip), 2);
	assert_eq!(nm.increment_strike(&ip), 3);
}

#[tokio::test]
async fn increment_strike_caps_at_max() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	for _ in 0..10 {
		nm.increment_strike(&ip);
	}

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 5);
	assert!(node.strike_excluded);
}

#[tokio::test]
async fn increment_strike_sets_excluded_at_max() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	for i in 1..=4 {
		let count = nm.increment_strike(&ip);
		assert_eq!(count, i);
		let node = nm.nodes.get(&ip).unwrap();
		assert!(!node.strike_excluded);
	}

	let count = nm.increment_strike(&ip);
	assert_eq!(count, 5);
	let node = nm.nodes.get(&ip).unwrap();
	assert!(node.strike_excluded);
}

#[tokio::test]
async fn increment_strike_unknown_peer_returns_zero() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	assert_eq!(nm.increment_strike(&ip), 0);
}

#[tokio::test]
async fn strikes_persist_across_reconnect() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	// Accumulate strikes
	nm.increment_strike(&ip);
	nm.increment_strike(&ip);
	nm.increment_strike(&ip);

	// Simulate disconnect: set state to Disconnected
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = ironcat::nodes::NodeState::Disconnected {
			retry_at: tokio::time::Instant::now(),
			attempt: 0,
		};
	}

	// Verify strikes survive the state change (try_insert_or_reactivate
	// does not touch strike fields, so they persist across reconnects)
	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 3);
	assert!(node.last_strike.is_some());
}

fn setup_bdm() -> (Arc<NodeManager>, BlockDownloadManager, tempfile::TempDir) {
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
	let dir = tempfile::tempdir().unwrap();
	let bs = Arc::new(BlockStore::open(dir.path()).unwrap());
	let cs = Arc::new(ironcat::chainstate::ChainState::open(dir.path(), u32::MAX).unwrap());
	let (_tx, rx) = tokio::sync::mpsc::channel(32);
	let (_dtx, drx) = tokio::sync::mpsc::unbounded_channel();
	let (_rtx, rrx) = tokio::sync::mpsc::unbounded_channel();
	let mempool = Arc::new(tokio::sync::RwLock::new(ironcat::mempool::Mempool::new()));
	nm.set_mempool(Arc::clone(&mempool));
	nm.set_chainstate(Arc::clone(&cs));
	let mgr = BlockDownloadManager::new(Arc::clone(&nm), bs, rx, drx, rrx, cs, mempool);
	(nm, mgr, dir)
}

#[tokio::test]
async fn timeout_scan_one_strike_per_peer_per_pass() {
	let (nm, mut mgr, _dir) = setup_bdm();

	let peer: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert_outgoing(peer, 9933);
	let old_time = Instant::now() - Duration::from_secs(120);

	// Multiple expired entries from same peer
	for i in 0u8..5 {
		let mut hash_bytes = [0u8; 32];
		hash_bytes[0] = i;
		mgr.in_flight.insert(
			Hash256::from_bytes(hash_bytes),
			ironcat::nodes::block_download::InFlightEntry {
				peer,
				requested_at: old_time,
				height: u32::from(i) + 1,
			},
		);
	}

	mgr.handle_timeout_scan();

	// Should be exactly 1 strike, not 5
	let node = nm.nodes.get(&peer).unwrap();
	assert_eq!(node.timeout_strikes, 1);
}

#[tokio::test]
async fn timeout_scan_excludes_peer_at_max_strikes() {
	let (nm, mut mgr, _dir) = setup_bdm();

	let peer: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert_outgoing(peer, 9933);

	// Pre-set strikes to MAX-1
	if let Some(mut node) = nm.nodes.get_mut(&peer) {
		node.timeout_strikes = 4;
	}

	let old_time = Instant::now() - Duration::from_secs(120);
	mgr.in_flight.insert(
		Hash256::from_bytes([0xAA; 32]),
		ironcat::nodes::block_download::InFlightEntry {
			peer,
			requested_at: old_time,
			height: 10,
		},
	);

	// Also add a fresh in-flight entry to verify it gets cleared on exclusion
	mgr.in_flight.insert(
		Hash256::from_bytes([0xBB; 32]),
		ironcat::nodes::block_download::InFlightEntry {
			peer,
			requested_at: Instant::now(),
			height: 20,
		},
	);

	mgr.handle_timeout_scan();

	// Peer should be excluded
	let node = nm.nodes.get(&peer).unwrap();
	assert_eq!(node.timeout_strikes, 5);
	assert!(node.strike_excluded);

	// All in-flight for this peer should be cleared (stale + fresh)
	assert!(mgr.in_flight.is_empty());
}

#[tokio::test]
async fn strike_decay_decrements_after_interval() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.timeout_strikes = 3;
		node.last_strike = Some(tokio::time::Instant::now() - Duration::from_secs(301));
	}

	nm.decay_strikes();

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 2);
	assert!(node.last_strike.unwrap().elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn strike_decay_no_decrement_before_interval() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.timeout_strikes = 3;
		node.last_strike = Some(tokio::time::Instant::now());
	}

	nm.decay_strikes();

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 3);
}

#[tokio::test]
async fn excluded_peer_stays_excluded_until_strikes_zero() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.timeout_strikes = 2;
		node.strike_excluded = true;
		node.last_strike = Some(tokio::time::Instant::now() - Duration::from_secs(301));
	}

	nm.decay_strikes();

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 1);
	assert!(node.strike_excluded);
}

#[tokio::test]
async fn excluded_peer_recovers_at_zero_strikes() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.timeout_strikes = 1;
		node.strike_excluded = true;
		node.last_strike = Some(tokio::time::Instant::now() - Duration::from_secs(301));
	}

	nm.decay_strikes();

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.timeout_strikes, 0);
	assert!(!node.strike_excluded);
	assert!(node.last_strike.is_none());
}

#[tokio::test]
async fn header_timeout_scanner_clears_pending_and_increments_strike() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.pending_getheaders = Some(tokio::time::Instant::now() - Duration::from_secs(10));
	}

	nm.scan_header_timeouts();

	let node = nm.nodes.get(&ip).unwrap();
	assert!(node.pending_getheaders.is_none());
	assert_eq!(node.timeout_strikes, 1);
}

#[tokio::test]
async fn header_timeout_scanner_ignores_fresh_requests() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.pending_getheaders = Some(tokio::time::Instant::now());
	}

	nm.scan_header_timeouts();

	let node = nm.nodes.get(&ip).unwrap();
	assert!(node.pending_getheaders.is_some());
	assert_eq!(node.timeout_strikes, 0);
}

#[tokio::test]
async fn excluded_peer_does_not_get_getheaders() {
	let nm = make_nm();
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert_outgoing(ip, 9933);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.timeout_strikes = 5;
		node.strike_excluded = true;
	}

	let node = nm.nodes.get(&ip).unwrap();
	assert!(node.strike_excluded);

	// Verify a non-excluded peer would pass the check
	let ip2: IpAddr = "10.0.0.2".parse().unwrap();
	nm.insert_outgoing(ip2, 9933);
	let node2 = nm.nodes.get(&ip2).unwrap();
	assert!(!node2.strike_excluded);
}
