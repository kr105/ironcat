// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use siphasher::sip::SipHasher13;
use tokio::time::Instant;
use tracing::{debug, info};

use super::{
	ADDR_RELAY_MAX_ENTRIES, ADDR_RELAY_PEER_COUNT, ADDR_TOKEN_CAPACITY, ADDR_TOKEN_RATE, ConnectionType,
	GETADDR_RECENT_WINDOW, MAX_ADDR_KNOWN, NodeManager, NodeState,
};
use crate::network::{
	NetworkAddress, SharedTcpWriter, SharedTcpWriterExt,
	message_addr::{AddrEntry, MessageAddr},
};
use crate::utils::{is_recently_active, is_routable, unix_now};

/// Computes a deterministic score for relay peer selection
///
/// Uses `SipHash-1-3` keyed by `relay_key` to produce a stable ranking of peers
/// for a given address within a 24-hour time bucket. Same inputs always produce same output
pub fn compute_relay_score(relay_key: u64, addr_hash: u64, time_bucket: u64, peer_hash: u64) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut hasher = SipHasher13::new_with_keys(relay_key, 0);
	addr_hash.hash(&mut hasher);
	time_bucket.hash(&mut hasher);
	peer_hash.hash(&mut hasher);
	hasher.finish()
}

/// Relays addr entries to the top-scoring outgoing peers
///
/// For each recently-active entry, selects the best peers by deterministic
/// hash score and sends a single-entry addr message to each
async fn relay_addr(node_manager: &Arc<NodeManager>, source_address: &IpAddr, entries: &[&AddrEntry]) {
	if entries.is_empty() {
		return;
	}

	// unix_now() returns seconds; 86400 seconds per day
	#[allow(clippy::arithmetic_side_effects)]
	let time_bucket = unix_now() / (24 * 60 * 60);

	// Collect connected outgoing peers (excluding source), releasing DashMap lock
	let peers: Vec<(IpAddr, SharedTcpWriter)> = node_manager
		.nodes
		.iter()
		.filter_map(|entry| {
			let node = entry.value();
			if *entry.key() == *source_address {
				return None;
			}
			if node.connection_type != ConnectionType::Outgoing {
				return None;
			}
			if let NodeState::Connected { ref writer } = node.state {
				Some((*entry.key(), Arc::clone(writer)))
			} else {
				None
			}
		})
		.collect();

	if peers.is_empty() {
		return;
	}

	for entry in entries {
		// Hash the IP for a stable addr identifier
		let addr_hash = {
			use std::hash::{Hash, Hasher};
			let mut hasher = SipHasher13::new_with_keys(node_manager.relay_key, 0);
			entry.address.address.hash(&mut hasher);
			hasher.finish()
		};

		// Serialize once per entry, reuse for all peers
		let relay_entry = AddrEntry {
			timestamp: entry.timestamp,
			address: NetworkAddress {
				services: entry.address.services,
				address: entry.address.address,
				port: entry.address.port,
			},
		};
		let msg = MessageAddr::from_entries(vec![relay_entry]);
		let payload = msg.to_bytes();

		// Score each peer and sort descending
		let mut scored: Vec<(usize, u64)> = peers
			.iter()
			.enumerate()
			.map(|(idx, (ip, _))| {
				let peer_hash = {
					use std::hash::{Hash, Hasher};
					let mut hasher = SipHasher13::new_with_keys(node_manager.relay_key, 0);
					ip.hash(&mut hasher);
					hasher.finish()
				};
				(
					idx,
					compute_relay_score(node_manager.relay_key, addr_hash, time_bucket, peer_hash),
				)
			})
			.collect();

		scored.sort_by(|a, b| b.1.cmp(&a.1));

		// Relay to top ADDR_RELAY_PEER_COUNT peers
		for &(idx, _) in scored.iter().take(ADDR_RELAY_PEER_COUNT) {
			// idx is bounded by peers.len() from the enumerate above
			#[allow(clippy::indexing_slicing)]
			let (ref peer_ip, ref writer) = peers[idx];

			// Check and update addr_known under DashMap lock
			// Clear the set when the 24h time bucket rotates to prevent unbounded growth
			let already_known = if let Some(mut node) = node_manager.nodes.get_mut(peer_ip) {
				if node.addr_known_bucket != time_bucket {
					node.addr_known.clear();
					node.addr_known_bucket = time_bucket;
				}
				// Cap addr_known to prevent unbounded growth within a 24h bucket
				if node.addr_known.len() >= MAX_ADDR_KNOWN {
					true
				} else {
					!node.addr_known.insert(entry.address.address)
				}
			} else {
				continue;
			};

			if already_known {
				continue;
			}

			if let Err(e) = writer.send_message("addr", &payload).await {
				debug!("Failed to relay addr to {}: {}", peer_ip, e);
			}
		}
	}
}

/// Refills a peer's addr token bucket based on elapsed time
///
/// Returns the updated token count, capped at `ADDR_TOKEN_CAPACITY`
pub const fn refill_addr_tokens(tokens: f64, elapsed: Duration) -> f64 {
	// Both operands are finite and bounded; result is capped by min()
	#[allow(clippy::arithmetic_side_effects, clippy::float_arithmetic)]
	elapsed
		.as_secs_f64()
		.mul_add(ADDR_TOKEN_RATE, tokens)
		.min(ADDR_TOKEN_CAPACITY)
}

/// Handles an incoming addr message containing peer addresses
pub(super) async fn handle_addr(node_manager: &Arc<NodeManager>, address: &IpAddr, payload: &[u8]) -> Result<()> {
	let msg = MessageAddr::from_bytes(payload).context("failed to parse addr message")?;

	debug!(
		"Received addr message with {} addresses from {}",
		msg.entries().len(),
		address
	);

	// Rate limiting: refill tokens, check freshness before deducting
	let mut recently_active_indices: Vec<usize> = Vec::new();
	{
		let Some(mut node) = node_manager.nodes.get_mut(address) else {
			return Ok(());
		};

		let now_instant = Instant::now();
		let elapsed = now_instant.duration_since(node.last_token_refill);
		node.addr_tokens = refill_addr_tokens(node.addr_tokens, elapsed);
		node.last_token_refill = now_instant;

		for (i, entry) in msg.entries().iter().enumerate() {
			// Check freshness before consuming a token so stale entries don't drain budget
			if !is_recently_active(entry.timestamp) {
				debug!(
					"Addr: {} filtered out (timestamp={}, not recently active)",
					entry.address.address, entry.timestamp
				);
				continue;
			}

			if node.addr_tokens >= 1.0 {
				// Finite f64 subtraction; both operands are bounded by ADDR_TOKEN_CAPACITY
				#[allow(clippy::arithmetic_side_effects, clippy::float_arithmetic)]
				{
					node.addr_tokens -= 1.0;
				}
				recently_active_indices.push(i);
			} else {
				debug!(
					"Rate limited addr entry {} from {} (tokens exhausted)",
					entry.address.address, address
				);
			}
		}
	}

	if recently_active_indices.is_empty() {
		return Ok(());
	}

	for &i in &recently_active_indices {
		// msg.entries() is bounded by 1000 (validated in from_bytes), i < entries.len()
		#[allow(clippy::indexing_slicing)]
		let entry = &msg.entries()[i];
		let entry_ip = entry.address.address;

		// Reject non-routable IPs to prevent internal network probing
		if !is_routable(entry_ip) {
			debug!("Addr: {} filtered out (not routable)", entry_ip);
			continue;
		}

		// Reject port 0 which has undefined connect behavior
		if entry.address.port == 0 {
			debug!("Addr: {} filtered out (port 0)", entry_ip);
			continue;
		}

		// Try to revive Dead nodes with a newer timestamp
		if node_manager.revive_if_newer(&entry_ip, entry.address.port, entry.timestamp) {
			info!("Revived dead node {} with newer addr timestamp", entry_ip);
			continue;
		}

		// Otherwise try to insert as new outgoing node
		debug!("Addr: {} is recently active, adding", entry.address.address);
		node_manager.insert_outgoing(entry.address.address, entry.address.port);
	}

	// Relay eligible entries to other peers
	// Only relay small batches from organic gossip (not getaddr responses)
	let should_relay = node_manager.nodes.get(address).is_some_and(|n| !n.sent_getaddr);

	if should_relay && msg.entries().len() <= ADDR_RELAY_MAX_ENTRIES {
		// recently_active_indices already filters by is_recently_active, no re-check needed
		let relay_entries: Vec<&AddrEntry> = recently_active_indices
			.iter()
			.map(|&i| {
				// Indices are bounded by msg.entries().len() from the rate limiting loop
				#[allow(clippy::indexing_slicing)]
				&msg.entries()[i]
			})
			.collect();

		relay_addr(node_manager, address, &relay_entries).await;
	}

	Ok(())
}

/// Handles an incoming getaddr request from a peer
pub(super) async fn handle_getaddr(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
	// Rate limit getaddr responses to 1 per minute
	{
		let Some(mut node) = node_manager.nodes.get_mut(address) else {
			return Ok(());
		};

		if let Some(last) = node.last_getaddr_response
			&& last.elapsed() < Duration::from_secs(60)
		{
			debug!("Rate limiting getaddr from {}", address);
			return Ok(());
		}

		node.last_getaddr_response = Some(Instant::now());
	}

	let now = unix_now();

	// Filter the node list so we share only recently active outgoing connections
	// Incoming connections use ephemeral OS ports -- their stored port is not their real listening port
	let filtered_nodes: Vec<NetworkAddress> = node_manager
		.nodes
		.iter()
		.filter(|entry| {
			let node = entry.value();
			node.state.is_connected()
				&& node.connection_type == ConnectionType::Outgoing
				&& (now.saturating_sub(node.last_seen) < GETADDR_RECENT_WINDOW)
		})
		.take(1000) // Protocol limits to maximum of 1000 entries per addr message
		.map(|entry| {
			let node = entry.value();
			NetworkAddress {
				services: node.services,
				address: *entry.key(),
				port: node.port,
			}
		})
		.collect();

	if filtered_nodes.is_empty() {
		debug!(
			"No recently active outgoing nodes to share for GetAddr from {}",
			address
		);
		return Ok(());
	}

	debug!("Sending list of {} nodes to {}", filtered_nodes.len(), address);

	let message_addr = MessageAddr::new(filtered_nodes);
	tcp_writer
		.send_message("addr", &message_addr.to_bytes())
		.await
		.context("failed to send addr")
}
