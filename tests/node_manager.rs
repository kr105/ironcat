// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode.
// DashMap guard drop order doesn't matter in synchronous test functions
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::significant_drop_tightening)]

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use ironcat::difficulty::ConsensusParams;
use ironcat::network::{ServiceMask, SharedTcpWriter};
use ironcat::nodes::{
	ADDR_TOKEN_CAPACITY, ADDR_TOKEN_INITIAL, BACKOFF_MAX_SECS, BanReason, ConnectionType, MAX_USER_AGENT_DISPLAY,
	NodeManager, NodeState, REAPER_SCAN_INTERVAL, ban_expires_at, calculate_backoff,
	handler_addr::{compute_relay_score, refill_addr_tokens},
	handler_version::sanitize_user_agent,
};
use ironcat::types::block::BlockHeader;
use ironcat::types::hash::Hash256;
use ironcat::utils::unix_now;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Dummy genesis header for tests that don't care about header data
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

#[test]
fn connectable_when_retry_time_passed() {
	let state = NodeState::Disconnected {
		retry_at: Instant::now() - Duration::from_secs(1),
		attempt: 0,
	};
	assert!(state.is_connectable());
}

#[test]
fn not_connectable_when_retry_pending() {
	let state = NodeState::Disconnected {
		retry_at: Instant::now() + Duration::from_secs(3600),
		attempt: 3,
	};
	assert!(!state.is_connectable());
}

#[test]
fn not_connectable_when_banned() {
	let state = NodeState::Banned {
		reason: BanReason::ProtocolViolation,
		created: 1_000_000,
		expires: 1_000_000 + 86_400,
	};
	assert!(!state.is_connectable());
}

#[test]
fn not_connectable_when_dead() {
	assert!(!NodeState::Dead.is_connectable());
}

#[test]
fn connecting_is_not_connected() {
	assert!(!NodeState::Connecting { since: Instant::now() }.is_connected());
}

#[test]
fn dead_is_not_connected() {
	assert!(!NodeState::Dead.is_connected());
}

#[test]
fn backoff_increases_with_attempts() {
	// Test multiple times to account for jitter
	for _ in 0..10 {
		let d0 = calculate_backoff(0);
		let d5 = calculate_backoff(5);
		// d0 should be around 30s +/-25% = 22.5-37.5s
		assert!(d0 >= Duration::from_secs(22));
		assert!(d0 <= Duration::from_secs(38));
		// d5 should be capped or near cap
		// BACKOFF_MAX_SECS + 25% jitter headroom + 1s tolerance
		#[allow(clippy::arithmetic_side_effects)]
		let max_with_jitter = Duration::from_secs(BACKOFF_MAX_SECS + BACKOFF_MAX_SECS / 4 + 1);
		assert!(d5 <= max_with_jitter);
	}
}

#[test]
fn backoff_caps_at_maximum() {
	let d = calculate_backoff(20); // way past cap
	// BACKOFF_MAX_SECS + 25% jitter headroom + 1s tolerance
	#[allow(clippy::arithmetic_side_effects)]
	let max_with_jitter = Duration::from_secs(BACKOFF_MAX_SECS + BACKOFF_MAX_SECS / 4 + 1);
	assert!(d <= max_with_jitter);
}

#[test]
fn banned_is_banned() {
	let now = unix_now();
	let state = NodeState::Banned {
		reason: BanReason::Misbehavior,
		created: now,
		expires: ban_expires_at(now),
	};
	assert!(state.is_banned());
}

#[test]
fn expired_ban_is_not_banned() {
	let state = NodeState::Banned {
		reason: BanReason::ProtocolViolation,
		created: 0,
		expires: 1, // expired long ago
	};
	assert!(!state.is_banned());
}

#[test]
fn connected_is_not_banned() {
	assert!(!NodeState::Connecting { since: Instant::now() }.is_banned());
}

#[test]
fn ban_reason_display() {
	assert_eq!(format!("{}", BanReason::ProtocolViolation), "protocol violation");
	assert_eq!(format!("{}", BanReason::Misbehavior), "misbehavior");
}

#[test]
fn refill_addr_tokens_adds_correctly() {
	let tokens = refill_addr_tokens(0.0, Duration::from_secs(50));
	// 50 * 0.1 = 5.0
	assert!((tokens - 5.0).abs() < f64::EPSILON);
}

#[test]
fn refill_addr_tokens_respects_capacity() {
	let tokens = refill_addr_tokens(999.0, Duration::from_secs(100));
	assert!((tokens - ADDR_TOKEN_CAPACITY).abs() < f64::EPSILON);
}

#[test]
fn refill_addr_tokens_zero_elapsed() {
	let tokens = refill_addr_tokens(500.0, Duration::from_secs(0));
	assert!((tokens - 500.0).abs() < f64::EPSILON);
}

#[test]
fn record_external_ip_vote_counts() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "8.8.8.8".parse().unwrap();
	nm.record_external_ip_vote(ip);
	nm.record_external_ip_vote(ip);
	nm.record_external_ip_vote(ip);
	assert_eq!(nm.get_external_ip(), Some(ip));
}

#[test]
fn get_external_ip_requires_three_votes() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "8.8.8.8".parse().unwrap();
	nm.record_external_ip_vote(ip);
	nm.record_external_ip_vote(ip);
	assert_eq!(nm.get_external_ip(), None);
}

#[test]
fn get_external_ip_returns_majority() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip_a: IpAddr = "8.8.8.8".parse().unwrap();
	let ip_b: IpAddr = "1.1.1.1".parse().unwrap();
	nm.record_external_ip_vote(ip_a);
	nm.record_external_ip_vote(ip_b);
	nm.record_external_ip_vote(ip_a);
	nm.record_external_ip_vote(ip_a);
	nm.record_external_ip_vote(ip_b);
	nm.record_external_ip_vote(ip_b);
	nm.record_external_ip_vote(ip_a);
	assert_eq!(nm.get_external_ip(), Some(ip_a));
}

#[test]
fn record_external_ip_vote_rejects_private() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let private_ip: IpAddr = "192.168.1.1".parse().unwrap();
	nm.record_external_ip_vote(private_ip);
	nm.record_external_ip_vote(private_ip);
	nm.record_external_ip_vote(private_ip);
	assert_eq!(nm.get_external_ip(), None);
}

#[test]
fn relay_score_is_deterministic() {
	let score_a = compute_relay_score(12345, 0xDEAD_BEEF, 1000, 42);
	let score_b = compute_relay_score(12345, 0xDEAD_BEEF, 1000, 42);
	assert_eq!(score_a, score_b);
}

#[test]
fn relay_score_varies_by_peer() {
	let score_a = compute_relay_score(12345, 0xDEAD_BEEF, 1000, 1);
	let score_b = compute_relay_score(12345, 0xDEAD_BEEF, 1000, 2);
	assert_ne!(score_a, score_b);
}

#[test]
fn relay_score_rotates_daily() {
	let score_day1 = compute_relay_score(12345, 0xDEAD_BEEF, 0, 42);
	let score_day2 = compute_relay_score(12345, 0xDEAD_BEEF, 1, 42);
	assert_ne!(score_day1, score_day2);
}

#[test]
fn has_incoming_connected_detects_incoming() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 12345, ConnectionType::Incoming);

	// Node starts in Handshaking, not Connected -- should return false
	assert!(!nm.has_incoming_connected());
}

#[test]
fn addr_known_dedup_prevents_duplicate_insert() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// First insert returns true (new)
	let first = nm.nodes.get_mut(&ip).unwrap().addr_known.insert(ip);
	assert!(first);

	// Second insert returns false (already known)
	let second = nm.nodes.get_mut(&ip).unwrap().addr_known.insert(ip);
	assert!(!second);
}

#[test]
fn addr_known_clears_on_bucket_rotation() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Insert an addr and set a bucket
	{
		let mut node = nm.nodes.get_mut(&ip).unwrap();
		node.addr_known.insert("1.2.3.4".parse::<IpAddr>().unwrap());
		node.addr_known_bucket = 100;
	}

	// Simulate bucket rotation by checking a different bucket
	{
		let mut node = nm.nodes.get_mut(&ip).unwrap();
		let new_bucket = 101;
		if node.addr_known_bucket != new_bucket {
			node.addr_known.clear();
			node.addr_known_bucket = new_bucket;
		}
		assert!(node.addr_known.is_empty());
	}
}

#[test]
fn sent_getaddr_defaults_to_false() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	let node = nm.nodes.get(&ip).unwrap();
	assert!(!node.sent_getaddr);
}

#[test]
fn sent_getaddr_can_be_set() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	{
		let mut node = nm.nodes.get_mut(&ip).unwrap();
		node.sent_getaddr = true;
	}
	let node = nm.nodes.get(&ip).unwrap();
	assert!(node.sent_getaddr);
}

#[test]
fn new_node_starts_with_initial_token_bucket() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	let node = nm.nodes.get(&ip).unwrap();
	assert!((node.addr_tokens - ADDR_TOKEN_INITIAL).abs() < f64::EPSILON);
}

#[test]
fn relay_score_same_key_different_addresses_differ() {
	let key = 42;
	let bucket = 100;
	let peer = 1;
	let score_a = compute_relay_score(key, 111, bucket, peer);
	let score_b = compute_relay_score(key, 222, bucket, peer);
	assert_ne!(score_a, score_b);
}

#[test]
fn same_ip_different_port_deduplicates() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();

	assert!(nm.insert(ip, 9933, ConnectionType::Outgoing));
	assert!(!nm.insert(ip, 8888, ConnectionType::Outgoing));
	assert_eq!(nm.nodes.len(), 1);
}

#[test]
fn insert_updates_port_when_disconnected() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Disconnected {
			retry_at: Instant::now(),
			attempt: 0,
		};
	}

	let result = nm.insert(ip, 8888, ConnectionType::Outgoing);
	assert!(!result, "should return false for existing node");

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 8888);
}

#[test]
fn insert_updates_port_when_dead() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Dead;
	}

	let result = nm.insert(ip, 7777, ConnectionType::Outgoing);
	assert!(!result);

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 7777);
}

#[test]
fn insert_ignores_when_connecting() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Node starts in Connecting state
	let result = nm.insert(ip, 8888, ConnectionType::Outgoing);
	assert!(!result);

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 9933, "port should not change when connecting");
}

#[test]
fn insert_incoming_new_node() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();

	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_some());

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 54321);
	assert_eq!(node.connection_type, ConnectionType::Incoming);
	assert!(matches!(node.state, NodeState::Handshaking { .. }));
}

#[test]
fn insert_incoming_replaces_disconnected() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Disconnected {
			retry_at: Instant::now(),
			attempt: 0,
		};
	}

	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_some());

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 54321);
	assert_eq!(node.connection_type, ConnectionType::Incoming);
	assert!(matches!(node.state, NodeState::Handshaking { .. }));
}

#[test]
fn insert_incoming_replaces_dead() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Dead;
	}

	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_some());

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 54321);
	assert_eq!(node.connection_type, ConnectionType::Incoming);
	assert!(matches!(node.state, NodeState::Handshaking { .. }));
}

#[test]
fn insert_incoming_rejects_when_connecting() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Node starts in Connecting -- should reject incoming
	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_none());

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 9933);
}

#[test]
fn insert_ignores_when_banned() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Banned {
			reason: BanReason::ProtocolViolation,
			created: 1_000_000,
			expires: 1_000_000 + 86_400,
		};
	}

	let result = nm.insert(ip, 8888, ConnectionType::Outgoing);
	assert!(!result);

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 9933, "port should not change when banned");
}

#[test]
fn insert_incoming_rejects_when_banned() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Banned {
			reason: BanReason::ProtocolViolation,
			created: 1_000_000,
			expires: 1_000_000 + 86_400,
		};
	}

	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_none(), "should reject incoming for banned node");
}

#[tokio::test]
async fn insert_ignores_when_connected() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Create a real TCP pair to get an OwnedWriteHalf
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
	let (_, write_half) = stream.into_split();
	let writer: SharedTcpWriter = Arc::new(Mutex::new(write_half));

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Connected { writer };
	}

	let result = nm.insert(ip, 8888, ConnectionType::Outgoing);
	assert!(!result);

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 9933, "port should not change when connected");
}

#[tokio::test]
async fn insert_incoming_rejects_when_connected() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
	let (_, write_half) = stream.into_split();
	let writer: SharedTcpWriter = Arc::new(Mutex::new(write_half));

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Connected { writer };
	}

	let result = nm.insert_incoming(ip, 54321);
	assert!(result.is_none(), "should reject incoming when node is connected");

	let node = nm.nodes.get(&ip).unwrap();
	assert_eq!(node.port, 9933, "port should not change");
}

#[test]
fn collect_peers_includes_connected_peer() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Simulate a peer that completed handshake: set version and last_seen
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.version = 70003;
		node.last_seen = 1_700_000_000;
		node.user_agent = "/Test:1.0/".to_string();
		node.height = 100;
		node.services = ServiceMask::NODE_NETWORK_LIMITED;
	}

	let db = nm.collect_peers_for_save();
	assert_eq!(db.peers.len(), 1);
	assert_eq!(db.peers[0].port, 9933);
	assert_eq!(db.peers[0].height, 100);
}

#[test]
fn collect_peers_excludes_unconnected_loaded_peer() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Simulate a peer loaded from disk: has last_seen but version stays 0
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.last_seen = 1_700_000_000;
	}

	let db = nm.collect_peers_for_save();
	assert!(db.peers.is_empty(), "peer with version=0 should not be saved");
}

#[test]
fn collect_peers_excludes_incoming() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Incoming);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.version = 70003;
		node.last_seen = 1_700_000_000;
	}

	let db = nm.collect_peers_for_save();
	assert!(db.peers.is_empty(), "incoming peers should not be saved");
}

#[test]
fn collect_peers_includes_disconnected_peer_with_version() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Peer connected once (version set), now disconnected and retrying.
	// version_received may be false but version field persists
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.version = 70003;
		node.last_seen = 1_700_000_000;
		node.version_received = false;
		node.state = NodeState::Disconnected {
			retry_at: Instant::now(),
			attempt: 1,
		};
	}

	let db = nm.collect_peers_for_save();
	assert_eq!(db.peers.len(), 1, "disconnected peer with version>0 should be saved");
}

#[tokio::test(start_paused = true)]
async fn reaper_clears_expired_bans() {
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	// Set an already-expired ban
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Banned {
			reason: BanReason::ProtocolViolation,
			created: 0,
			expires: 1, // expired long ago
		};
	}

	// Verify the ban shows as expired
	let node = nm.nodes.get(&ip).unwrap();
	assert!(!node.state.is_banned(), "ban should be expired");
	drop(node);

	// Run one reaper cycle -- with paused time, sleep auto-advances
	// past the 30s reaper interval, letting the scan execute
	let nm_clone = Arc::clone(&nm);
	let reaper = tokio::spawn(async move { nm_clone.run_reaper().await });
	tokio::time::sleep(REAPER_SCAN_INTERVAL + Duration::from_secs(1)).await;
	reaper.abort();

	// The expired ban should have been cleared to Dead
	let node = nm.nodes.get(&ip).unwrap();
	assert!(
		matches!(node.state, NodeState::Dead),
		"expired ban should become Dead, got {}",
		node.state
	);
}

#[test]
fn collect_bans_excludes_expired() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "1.2.3.4".parse().unwrap();
	nm.insert(ip, 9933, ConnectionType::Outgoing);

	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Banned {
			reason: BanReason::Misbehavior,
			created: 0,
			expires: 1, // expired
		};
	}

	let db = nm.collect_bans_for_save();
	assert!(db.bans.is_empty(), "expired bans should not be saved");
}

#[test]
fn sanitize_user_agent_strips_control_chars() {
	assert_eq!(sanitize_user_agent("/Satoshi:0.1/"), "/Satoshi:0.1/");
}

#[test]
fn sanitize_user_agent_strips_non_ascii() {
	assert_eq!(sanitize_user_agent("/Test\x1b[31m:evil/"), "/Test[31m:evil/");
}

#[test]
fn sanitize_user_agent_trims_whitespace() {
	assert_eq!(sanitize_user_agent("  /Satoshi:0.1/  "), "/Satoshi:0.1/");
}

#[test]
fn sanitize_user_agent_all_spaces_becomes_empty() {
	assert_eq!(sanitize_user_agent("       "), "");
}

#[test]
fn sanitize_user_agent_truncates_long_input() {
	let long = "A".repeat(300);
	let result = sanitize_user_agent(&long);
	assert_eq!(result.len(), MAX_USER_AGENT_DISPLAY);
}

#[test]
fn load_saved_peers_skips_unknown_version() {
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
	let db = ironcat::storage::peers::PeerDb {
		version: 999,
		peers: vec![ironcat::storage::peers::SavedPeer {
			ip: "1.2.3.4".parse().unwrap(),
			port: 9933,
			services: 0,
			last_seen: 0,
			user_agent: String::new(),
			height: 0,
		}],
	};
	nm.load_saved_peers(&db);
	assert!(nm.nodes.is_empty(), "should skip peers with unknown version");
}

#[test]
fn load_saved_bans_skips_unknown_version() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let db = ironcat::storage::bans::BanDb {
		version: 999,
		bans: vec![ironcat::storage::bans::SavedBan {
			ip: "1.2.3.4".parse().unwrap(),
			reason: "test".to_string(),
			created: 0,
			expires: u64::MAX,
		}],
	};
	nm.load_saved_bans(&db);
	assert!(nm.nodes.is_empty(), "should skip bans with unknown version");
}

#[test]
fn incoming_cooldown_rejects_rapid_reconnect() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();

	// First connection should succeed
	let guard = nm.insert_incoming(ip, 54321);
	assert!(guard.is_some());
	drop(guard);

	// Make the node Dead so it's eligible again
	if let Some(mut node) = nm.nodes.get_mut(&ip) {
		node.state = NodeState::Dead;
	}

	// Second connection within cooldown should be rejected
	let guard2 = nm.insert_incoming(ip, 54322);
	assert!(guard2.is_none(), "should reject per-IP cooldown");
}

#[test]
fn incoming_guard_decrements_on_drop() {
	use std::sync::atomic::Ordering;

	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let ip: IpAddr = "10.0.0.1".parse().unwrap();

	let guard = nm.insert_incoming(ip, 54321);
	assert!(guard.is_some());
	assert_eq!(nm.incoming_count.load(Ordering::Acquire), 1);

	drop(guard);
	assert_eq!(nm.incoming_count.load(Ordering::Acquire), 0);
}

#[test]
fn block_store_none_by_default() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	assert!(nm.block_store().is_none());
}

#[test]
fn block_store_set_and_get() {
	let nm = NodeManager::new(test_genesis(), ConsensusParams::mainnet());
	let dir = tempfile::tempdir().unwrap();
	let store = Arc::new(ironcat::storage::block_store::BlockStore::open(dir.path()).unwrap());
	nm.set_block_store(Arc::clone(&store));
	assert!(nm.block_store().is_some());
}
