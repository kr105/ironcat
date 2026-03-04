// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use rand::RngCore;
use siphasher::sip::SipHasher13;
use std::{
	collections::HashSet,
	ffi::CStr,
	fmt,
	net::IpAddr,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	time::Duration,
};
use tokio::{
	io::{self, AsyncWriteExt},
	net::TcpStream,
	sync::Mutex,
	time::{sleep, timeout, Instant},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
	dns::DEFAULT_PORT,
	headers::HeaderStore,
	network::{
		message_addr::{AddrEntry, MessageAddr},
		message_getheaders::MessageGetHeaders,
		message_headers::{MessageHeaders, MAX_HEADERS_PER_MSG},
		message_inv::{InvType, MessageInv},
		message_version::MessageVersion,
		Message, NetworkAddress, NetworkCommand, NetworkQueue, ServiceMask, SharedTcpWriter, SharedTcpWriterExt,
	},
	types::{block::BlockHeader, hash::Hash256},
	utils::{is_recently_active, is_routable, unix_now, vec_to_u64_le},
};

/// Base delay for exponential backoff in seconds
const BACKOFF_BASE_SECS: u64 = 30;

/// Maximum backoff delay in seconds (30 minutes)
const BACKOFF_MAX_SECS: u64 = 30 * 60;

/// Maximum retry attempts before marking a node as dead
const MAX_RETRY_ATTEMPTS: u32 = 10;

/// Maximum number of nodes to track
const MAX_NODES: usize = 5000;

/// Maximum TCP connection attempts per cycle
const TCP_MAX_ATTEMPTS: u32 = 3;

/// Delay between TCP connection retries
const TCP_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Timeout for a single TCP connect attempt
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between keepalive pings
const PING_INTERVAL: Duration = Duration::from_secs(180);

/// How long a node can sit in Connecting before the reaper reclaims it
const STALE_CONNECTING_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a node can sit in Handshaking before the reaper reclaims it
const STALE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the reaper scans for retryable/stale nodes
const REAPER_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Window for "recently active" outgoing nodes in `GetAddr` responses (2 hours)
const GETADDR_RECENT_WINDOW: u64 = 60 * 60 * 2;

/// Maximum entries in an addr message eligible for relay
const ADDR_RELAY_MAX_ENTRIES: usize = 10;

/// Number of peers to relay each addr entry to
const ADDR_RELAY_PEER_COUNT: usize = 2;

/// Token bucket refill rate: 1 token per 10 seconds
const ADDR_TOKEN_RATE: f64 = 0.1;

/// Token bucket maximum capacity
const ADDR_TOKEN_CAPACITY: f64 = 1000.0;

/// Token bucket initial fill for new/reactivated connections
const ADDR_TOKEN_INITIAL: f64 = 10.0;

/// How often to announce our own address to peers
const SELF_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Maximum length of user agent string after sanitization
const MAX_USER_AGENT_DISPLAY: usize = 256;

/// Maximum number of entries in the external IP votes map
const MAX_EXTERNAL_IP_VOTES: usize = 100;

/// Maximum entries in a peer's `addr_known` set within a 24h bucket
const MAX_ADDR_KNOWN: usize = 5000;

/// Maximum entries in a peer's `inv_known` set before clearing
const MAX_INV_KNOWN: usize = 50_000;

/// Minimum protocol version that supports sendheaders (BIP 130)
#[allow(dead_code)] // used in subsequent headers-sync tasks
const SENDHEADERS_VERSION: u32 = 70012;

/// Maximum concurrent incoming connections
const MAX_INCOMING_CONNECTIONS: usize = 125;

/// Cooldown period before the same IP can make a new incoming connection
const INCOMING_IP_COOLDOWN: Duration = Duration::from_secs(30);

/// How long a ban lasts in seconds (24 hours)
const BAN_DURATION_SECS: u64 = 24 * 60 * 60;

/// How long a connection can sit idle before being dropped (2x `PING_INTERVAL`)
const IDLE_TIMEOUT: Duration = Duration::from_secs(360);

/// Reason why a node was banned (bans expire after `BAN_DURATION_SECS`)
#[derive(Debug)]
pub enum BanReason {
	/// Sent version twice, self-connection, etc
	ProtocolViolation,
	/// Malformed messages, invalid data
	Misbehavior,
}

impl fmt::Display for BanReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::ProtocolViolation => write!(f, "protocol violation"),
			Self::Misbehavior => write!(f, "misbehavior"),
		}
	}
}

/// Returns the ban expiration timestamp given a creation time
///
/// All ban reasons share the same duration (`BAN_DURATION_SECS`)
pub const fn ban_expires_at(created: u64) -> u64 {
	created.saturating_add(BAN_DURATION_SECS)
}

/// Represents the connection state of a node
#[derive(Debug)]
pub enum NodeState {
	/// TCP connect in progress
	Connecting { since: Instant },
	/// TCP connected, version handshake in progress
	Handshaking { since: Instant },
	/// Fully connected and operational
	Connected { writer: SharedTcpWriter },
	/// Disconnected, will retry after `retry_at` with exponential backoff
	Disconnected { retry_at: Instant, attempt: u32 },
	/// Exhausted all retry attempts -- can be revived by a fresh addr message
	Dead,
	/// Banned -- will expire after `BAN_DURATION_SECS`
	Banned {
		reason: BanReason,
		created: u64,
		expires: u64,
	},
}

impl NodeState {
	/// Whether this node is ready for a new connection attempt right now
	pub fn is_connectable(&self) -> bool {
		matches!(self, Self::Disconnected { retry_at, .. } if *retry_at <= Instant::now())
	}

	/// Whether this node has a fully established connection
	pub const fn is_connected(&self) -> bool {
		matches!(self, Self::Connected { .. })
	}

	/// Whether this node is currently banned (ban may have expired)
	pub fn is_banned(&self) -> bool {
		match self {
			Self::Banned { expires, .. } => crate::utils::unix_now() < *expires,
			_ => false,
		}
	}
}

impl fmt::Display for NodeState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Connecting { .. } => write!(f, "Connecting"),
			Self::Handshaking { .. } => write!(f, "Handshaking"),
			Self::Connected { .. } => write!(f, "Connected"),
			Self::Disconnected { attempt, .. } => write!(f, "Disconnected({attempt})"),
			Self::Dead => write!(f, "Dead"),
			Self::Banned { reason, .. } => write!(f, "Banned({reason})"),
		}
	}
}

/// Calculates backoff duration with exponential growth and jitter
pub fn calculate_backoff(attempt: u32) -> Duration {
	let base = BACKOFF_BASE_SECS.saturating_mul(2u64.saturating_pow(attempt));
	let capped = base.min(BACKOFF_MAX_SECS);

	// Add +/-25% jitter to prevent thundering herd
	let jitter_range = capped / 4;
	let jitter_window = jitter_range.saturating_mul(2);
	let jitter_offset = if jitter_window > 0 {
		// Modulo by a nonzero value is safe, and wrapping is acceptable for RNG
		#[allow(clippy::arithmetic_side_effects)]
		{
			rand::thread_rng().next_u64() % jitter_window
		}
	} else {
		0
	};

	Duration::from_secs(capped.saturating_sub(jitter_range).saturating_add(jitter_offset))
}

/// Summary statistics about managed nodes
#[derive(Debug)]
pub struct NodeStats {
	/// Total number of tracked nodes
	pub total: usize,
	/// Number of nodes in Connected state
	pub connected: usize,
	/// Number of nodes not in Connected state
	pub disconnected: usize,
}

/// Direction of a node connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
	/// Peer connected to us
	Incoming,
	/// We connected to the peer
	Outgoing,
}

impl fmt::Display for ConnectionType {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Incoming => write!(f, "In"),
			Self::Outgoing => write!(f, "Out"),
		}
	}
}

/// Copy-friendly label for node state, avoiding String allocation under `DashMap` locks
#[derive(Debug, Clone, Copy)]
pub enum NodeStateLabel {
	/// TCP connect in progress
	Connecting,
	/// Version handshake in progress
	Handshaking,
	/// Fully connected
	Connected,
	/// Waiting for backoff retry
	Disconnected(u32),
	/// All retries exhausted
	Dead,
	/// Permanently banned
	Banned,
}

impl fmt::Display for NodeStateLabel {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Connecting => write!(f, "Connecting"),
			Self::Handshaking => write!(f, "Handshaking"),
			Self::Connected => write!(f, "Connected"),
			Self::Disconnected(attempt) => write!(f, "Disconnected({attempt})"),
			Self::Dead => write!(f, "Dead"),
			Self::Banned => write!(f, "Banned"),
		}
	}
}

impl NodeStateLabel {
	/// Creates a label from a `NodeState` reference
	const fn from_state(state: &NodeState) -> Self {
		match state {
			NodeState::Connecting { .. } => Self::Connecting,
			NodeState::Handshaking { .. } => Self::Handshaking,
			NodeState::Connected { .. } => Self::Connected,
			NodeState::Disconnected { attempt, .. } => Self::Disconnected(*attempt),
			NodeState::Dead => Self::Dead,
			NodeState::Banned { .. } => Self::Banned,
		}
	}
}

/// Lightweight snapshot of node data for display purposes
///
/// Contains only the fields needed by the TUI, without internal state
/// like `tcp_writer` or the full `NodeState` enum
pub struct NodeSnapshot {
	/// IP address of the node
	pub address: IpAddr,
	/// Listening port of the node
	pub port: u16,
	/// Last known block height
	pub height: i32,
	/// Whether this is an incoming or outgoing connection
	pub connection_type: ConnectionType,
	/// Human-readable state label for display
	pub state_label: NodeStateLabel,
}

/// Represents a node in the Catcoin network
#[allow(clippy::struct_excessive_bools)] // each bool is a distinct protocol flag, not a state machine
pub struct Node {
	/// Port number for this node's listening socket
	pub port: u16,

	/// Unix timestamp of the last message received from this node
	pub last_seen: u64,

	/// Protocol version reported by this node
	pub version: u32,

	/// Whether we have received a version message from this peer
	pub version_received: bool,

	/// Service flags advertised by this node
	pub services: ServiceMask,

	/// Unix timestamp from the node's version message
	pub timestamp: i64,

	/// User agent string reported by this node
	pub user_agent: String,

	/// Last known block height from this node
	pub height: i32,

	/// Whether this node wants transaction relay (BIP37)
	pub relay: bool,

	/// Direction of this connection (incoming or outgoing)
	pub connection_type: ConnectionType,

	/// Current connection lifecycle state
	pub state: NodeState,

	/// Addresses already relayed to this peer in the current 24h window
	pub addr_known: HashSet<IpAddr>,

	/// The 24h time bucket when `addr_known` was last cleared
	pub addr_known_bucket: u64,

	/// Token bucket: available tokens for rate limiting incoming addr entries
	pub addr_tokens: f64,

	/// Token bucket: last time tokens were refilled
	pub last_token_refill: Instant,

	/// Whether we sent getaddr to this peer (inhibits relay of their addr response)
	pub sent_getaddr: bool,

	/// When we last responded to a getaddr from this peer
	pub last_getaddr_response: Option<Instant>,

	/// IP voted by this peer in their version message, applied on verack
	pub pending_external_ip: Option<IpAddr>,

	/// Nonce of the last ping we sent to this peer, for pong validation
	pub last_ping_nonce: Option<u64>,

	/// Inventory hashes known to this peer (announced via inv)
	pub inv_known: HashSet<Hash256>,

	/// Whether this peer prefers block announcements via headers (BIP 130)
	pub prefer_headers: bool,
}

impl Node {
	/// Creates a new node with default field values
	fn new(port: u16, connection_type: ConnectionType, state: NodeState) -> Self {
		Self {
			port,
			last_seen: 0,
			height: 0,
			relay: false,
			services: ServiceMask::empty(),
			timestamp: 0,
			user_agent: String::new(),
			version: 0,
			version_received: false,
			connection_type,
			state,
			addr_known: HashSet::new(),
			addr_known_bucket: 0,
			addr_tokens: ADDR_TOKEN_INITIAL,
			last_token_refill: Instant::now(),
			sent_getaddr: false,
			last_getaddr_response: None,
			pending_external_ip: None,
			last_ping_nonce: None,
			inv_known: HashSet::new(),
			prefer_headers: false,
		}
	}
}

/// RAII guard that decrements the incoming connection counter on drop
///
/// Ensures the counter is always decremented even if the task panics
pub struct IncomingGuard {
	counter: Arc<AtomicUsize>,
}

impl Drop for IncomingGuard {
	fn drop(&mut self) {
		self.counter.fetch_sub(1, Ordering::Release);
	}
}

/// Manages all tracked nodes in the Catcoin P2P network
///
/// Uses `DashMap` for lock-free concurrent access across Tokio tasks.
/// The `my_nonce` field detects self-connections during handshake.
/// The `relay_key` seeds deterministic peer selection for addr relay
pub struct NodeManager {
	/// Concurrent map of all tracked nodes, keyed by IP address
	pub(crate) nodes: DashMap<IpAddr, Node>,

	/// Random nonce for self-connection detection
	pub my_nonce: u64,

	/// Per-session random key for deterministic addr relay peer selection
	pub(crate) relay_key: u64,

	/// Peer votes for our external IP address
	external_ip_votes: DashMap<IpAddr, u32>,

	/// Number of currently active incoming connections
	incoming_count: Arc<AtomicUsize>,

	/// Per-IP cooldown for incoming connections to prevent reconnect spam
	incoming_cooldowns: DashMap<IpAddr, Instant>,

	/// In-memory block header chain
	pub header_store: Arc<parking_lot::RwLock<HeaderStore>>,
}

impl NodeManager {
	/// Creates a new `NodeManager` with an empty node set and random nonce and relay key
	pub fn new(genesis_header: BlockHeader) -> Self {
		let mut rng = rand::thread_rng();
		Self {
			nodes: DashMap::new(),
			my_nonce: rng.next_u64(),
			relay_key: rng.next_u64(),
			external_ip_votes: DashMap::new(),
			incoming_count: Arc::new(AtomicUsize::new(0)),
			incoming_cooldowns: DashMap::new(),
			header_store: Arc::new(parking_lot::RwLock::new(HeaderStore::new(genesis_header))),
		}
	}

	/// Inserts a new node into the manager if it doesn't already exist
	///
	/// For occupied entries that are Disconnected or Dead, silently updates port
	/// and connection type. Returns true only if a new entry was inserted
	pub fn insert(&self, address: IpAddr, port: u16, connection_type: ConnectionType) -> bool {
		self.try_insert_or_reactivate(address, port, connection_type, false)
	}

	/// Inserts or reactivates a node for an incoming connection
	///
	/// If the node exists and is Disconnected or Dead, resets it to Handshaking.
	/// If the node exists and is active, returns None (reject the connection).
	/// If the node doesn't exist, inserts it in Handshaking state.
	/// Returns an `IncomingGuard` that auto-decrements the counter on drop
	pub fn insert_incoming(&self, address: IpAddr, port: u16) -> Option<IncomingGuard> {
		if self.incoming_count.load(Ordering::Acquire) >= MAX_INCOMING_CONNECTIONS {
			debug!(
				"Incoming connection limit reached ({}), rejecting {}:{}",
				MAX_INCOMING_CONNECTIONS, address, port
			);
			return None;
		}

		// Per-IP cooldown: reject if this IP connected too recently
		if let Some(last) = self.incoming_cooldowns.get(&address) {
			if last.elapsed() < INCOMING_IP_COOLDOWN {
				debug!("Rejecting {}:{} (per-IP cooldown)", address, port);
				return None;
			}
		}

		if self.try_insert_or_reactivate(address, port, ConnectionType::Incoming, true) {
			self.incoming_count.fetch_add(1, Ordering::Release);
			self.incoming_cooldowns.insert(address, Instant::now());
			Some(IncomingGuard {
				counter: Arc::clone(&self.incoming_count),
			})
		} else {
			None
		}
	}

	/// Shared insert/reactivate logic for both outgoing and incoming nodes
	///
	/// When `reset_handshake` is true (incoming connections), resets handshake
	/// fields on reactivation so stale state from a previous connection is cleared
	fn try_insert_or_reactivate(
		&self,
		address: IpAddr,
		port: u16,
		connection_type: ConnectionType,
		reset_handshake: bool,
	) -> bool {
		if self.nodes.len() >= MAX_NODES {
			debug!("Node limit reached ({}), not adding {}:{}", MAX_NODES, address, port);
			return false;
		}

		let entry = self.nodes.entry(address);

		match entry {
			dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
				let node = occupied.get_mut();
				if matches!(node.state, NodeState::Disconnected { .. } | NodeState::Dead) {
					node.port = port;
					node.connection_type = connection_type;

					if reset_handshake {
						node.state = NodeState::Handshaking { since: Instant::now() };
						node.version_received = false;
						node.sent_getaddr = false;
						node.addr_tokens = ADDR_TOKEN_INITIAL;
						node.last_token_refill = Instant::now();
					}

					reset_handshake
				} else {
					false
				}
			}
			dashmap::mapref::entry::Entry::Vacant(vacant) => {
				let initial_state = if reset_handshake {
					NodeState::Handshaking { since: Instant::now() }
				} else {
					match connection_type {
						ConnectionType::Outgoing => NodeState::Connecting { since: Instant::now() },
						ConnectionType::Incoming => NodeState::Handshaking { since: Instant::now() },
					}
				};

				vacant.insert(Node::new(port, connection_type, initial_state));
				true
			}
		}
	}

	/// Updates the `last_seen` timestamp for a node with the current time
	pub fn update_last_seen(&self, address: &IpAddr) {
		if let Some(mut node) = self.nodes.get_mut(address) {
			node.last_seen = unix_now();
		}
	}

	/// Updates the connection state of a node
	pub fn set_state(&self, address: &IpAddr, state: NodeState) {
		if let Some(mut node) = self.nodes.get_mut(address) {
			// Clear per-session state when the connection ends
			if matches!(state, NodeState::Disconnected { .. } | NodeState::Dead) {
				node.inv_known.clear();
			}
			node.state = state;
		}
	}

	/// Checks if the node is in a state where a connection attempt should proceed
	///
	/// Returns true for Connecting (we just set this state) or Disconnected with elapsed backoff
	pub fn should_attempt_connection(&self, address: &IpAddr) -> bool {
		if let Some(node) = self.nodes.get(address) {
			return matches!(node.state, NodeState::Connecting { .. }) || node.state.is_connectable();
		}

		false
	}

	/// Sends a ping message to a connected node and stores the nonce for pong validation
	pub async fn send_ping(&self, address: &IpAddr) {
		let nonce = rand::thread_rng().next_u64();

		let tcp_writer = if let Some(mut node) = self.nodes.get_mut(address) {
			let writer = if let NodeState::Connected { ref writer } = node.state {
				Arc::clone(writer)
			} else {
				debug!("Skipping ping to {}, not connected", address);
				return;
			};
			node.last_ping_nonce = Some(nonce);
			writer
		} else {
			debug!("Skipping ping to {}, node not found", address);
			return;
		};

		debug!("Sending ping to {}", address);

		if let Err(e) = tcp_writer.send_message("ping", &nonce.to_le_bytes()).await {
			warn!("Failed to send ping to {}: {}", address, e);
		}
	}

	/// Returns the number of nodes currently in Connected state
	pub fn connected_count(&self) -> usize {
		self.nodes.iter().filter(|e| e.value().state.is_connected()).count()
	}

	/// Returns stats and node snapshots in a single `DashMap` iteration
	///
	/// Only acquires shard locks once for both stats and snapshot data
	pub fn get_snapshot(&self) -> (NodeStats, Vec<NodeSnapshot>) {
		let mut connected: usize = 0;
		let snapshots: Vec<NodeSnapshot> = self
			.nodes
			.iter()
			.map(|entry| {
				let node = entry.value();
				if node.state.is_connected() {
					connected = connected.saturating_add(1);
				}
				NodeSnapshot {
					address: *entry.key(),
					port: node.port,
					height: node.height,
					connection_type: node.connection_type,
					state_label: NodeStateLabel::from_state(&node.state),
				}
			})
			.collect();

		let total = snapshots.len();
		let disconnected = total.saturating_sub(connected);

		(
			NodeStats {
				total,
				connected,
				disconnected,
			},
			snapshots,
		)
	}

	/// Background task that periodically scans for disconnected nodes ready for retry
	///
	/// Runs every 30 seconds and spawns connection tasks for nodes whose
	/// backoff period has elapsed. Collects ready and stale nodes in a single pass
	pub async fn run_reaper(self: Arc<Self>) {
		loop {
			tokio::time::sleep(REAPER_SCAN_INTERVAL).await;

			// Single-pass scan: collect both ready-for-retry and stale nodes,
			// plus count connected for stats
			let mut ready_nodes: Vec<(IpAddr, u32)> = Vec::new();
			let mut stale_nodes: Vec<IpAddr> = Vec::new();
			let mut expired_bans: Vec<IpAddr> = Vec::new();
			let mut total: usize = 0;
			let mut connected: usize = 0;

			for entry in &self.nodes {
				total = total.saturating_add(1);
				let node = entry.value();

				match &node.state {
					NodeState::Disconnected { attempt, .. } if node.state.is_connectable() => {
						ready_nodes.push((*entry.key(), *attempt));
					}
					NodeState::Connecting { since } if since.elapsed() > STALE_CONNECTING_TIMEOUT => {
						stale_nodes.push(*entry.key());
					}
					NodeState::Handshaking { since } if since.elapsed() > STALE_HANDSHAKE_TIMEOUT => {
						stale_nodes.push(*entry.key());
					}
					NodeState::Connected { .. } => {
						connected = connected.saturating_add(1);
					}
					NodeState::Banned { .. } if !node.state.is_banned() => {
						expired_bans.push(*entry.key());
					}
					_ => {}
				}
			}

			if !ready_nodes.is_empty() {
				info!("Reaper: {} nodes ready for retry", ready_nodes.len());
			}

			for (address, attempt) in ready_nodes {
				// Transition to Connecting before spawning, read port while locked
				let port = if let Some(mut node) = self.nodes.get_mut(&address) {
					// Double-check state hasn't changed since we collected
					if !node.state.is_connectable() {
						continue;
					}
					node.state = NodeState::Connecting { since: Instant::now() };
					node.port
				} else {
					continue;
				};

				let nm = Arc::clone(&self);
				tokio::spawn(async move {
					handle_node_connection(nm, address, port, attempt).await;
				});
			}

			for address in stale_nodes {
				warn!("Node {} stuck in stale state, scheduling retry", address);
				if let Some(mut node) = self.nodes.get_mut(&address) {
					if matches!(node.state, NodeState::Connecting { .. } | NodeState::Handshaking { .. }) {
						node.inv_known.clear();
						node.state = NodeState::Disconnected {
							retry_at: Instant::now(),
							attempt: 0,
						};
					}
				}
			}

			if !expired_bans.is_empty() {
				info!(count = expired_bans.len(), "Reaper: clearing expired bans");
			}
			for address in expired_bans {
				if let Some(mut node) = self.nodes.get_mut(&address) {
					if matches!(node.state, NodeState::Banned { .. }) && !node.state.is_banned() {
						node.state = NodeState::Dead;
					}
				}
			}

			// Evict external IP votes from peers that are no longer connected
			let connected_ips: HashSet<IpAddr> = self
				.nodes
				.iter()
				.filter(|e| e.value().state.is_connected())
				.map(|e| *e.key())
				.collect();
			self.external_ip_votes.retain(|ip, _| connected_ips.contains(ip));

			// Purge stale per-IP incoming cooldowns (older than 2x the cooldown period)
			self.incoming_cooldowns
				.retain(|_, instant| instant.elapsed() < INCOMING_IP_COOLDOWN.saturating_mul(2));

			debug!(
				"Reaper scan complete: {} total, {} connected, {} disconnected",
				total,
				connected,
				total.saturating_sub(connected)
			);
		}
	}

	/// Inserts a new outgoing node and spawns a connection task
	///
	/// Does nothing if the node already exists in the manager
	pub fn insert_outgoing(self: &Arc<Self>, address: IpAddr, port: u16) {
		if self.insert(address, port, ConnectionType::Outgoing) {
			let nm = Arc::clone(self);
			tokio::spawn(async move {
				handle_node_connection(nm, address, port, 0).await;
			});
		}
	}

	/// Shuts down all connected nodes cleanly by sending TCP FIN
	///
	/// Collects writers first to avoid holding `DashMap` locks across awaits
	pub async fn graceful_shutdown(&self) {
		let writers: Vec<(IpAddr, SharedTcpWriter)> = self
			.nodes
			.iter()
			.filter_map(|entry| {
				if let NodeState::Connected { ref writer } = entry.state {
					Some((*entry.key(), Arc::clone(writer)))
				} else {
					None
				}
			})
			.collect();

		let count = writers.len();
		if count == 0 {
			return;
		}

		info!(count, "Shutting down connected nodes");

		for (address, writer) in writers {
			if let Err(err) = writer.lock().await.shutdown().await {
				warn!(%address, error = %err, "Failed to shut down writer");
			}
		}

		info!("Graceful shutdown complete");
	}

	/// Records a peer's report of our external IP address
	///
	/// Non-routable IPs (private, loopback, link-local, etc) are silently ignored
	pub fn record_external_ip_vote(&self, ip: IpAddr) {
		if !is_routable(ip) {
			debug!(ip = %ip, "Ignoring non-routable external IP vote");
			return;
		}

		// Cap the votes map to prevent unbounded growth
		if self.external_ip_votes.len() >= MAX_EXTERNAL_IP_VOTES {
			debug!(ip = %ip, "External IP votes map full, ignoring vote");
			return;
		}

		self.external_ip_votes
			.entry(ip)
			.and_modify(|count| {
				*count = count.saturating_add(1);
			})
			.or_insert(1);
	}

	/// Returns our external IP if at least 3 peers agree and it has > 50% of votes
	///
	/// Returns the IP with the most votes, or None if no IP meets both thresholds
	pub fn get_external_ip(&self) -> Option<IpAddr> {
		let total_votes: u32 = self.external_ip_votes.iter().map(|entry| *entry.value()).sum();

		self.external_ip_votes
			.iter()
			.filter(|entry| {
				let votes = *entry.value();
				// total_votes is always >= votes, so * 2 won't overflow for realistic vote counts
				#[allow(clippy::arithmetic_side_effects)]
				let majority = votes * 2 > total_votes;
				votes >= 3 && majority
			})
			.max_by_key(|entry| *entry.value())
			.map(|entry| *entry.key())
	}

	/// Revives a Dead node if the given timestamp is newer than `last_seen`
	///
	/// Returns true if the node was revived, false otherwise
	pub fn revive_if_newer(self: &Arc<Self>, address: &IpAddr, port: u16, timestamp: u32) -> bool {
		if let Some(mut node) = self.nodes.get_mut(address) {
			if matches!(node.state, NodeState::Dead) && u64::from(timestamp) > node.last_seen {
				node.state = NodeState::Connecting { since: Instant::now() };
				node.last_seen = u64::from(timestamp);
				node.port = port;
				let revived_addr = *address;
				let revived_port = node.port;
				drop(node);

				// Spawn a connection task for the revived node
				let nm = Arc::clone(self);
				tokio::spawn(async move {
					handle_node_connection(nm, revived_addr, revived_port, 0).await;
				});

				return true;
			}
		}
		false
	}

	/// Returns true if at least one incoming peer is in Connected state
	pub fn has_incoming_connected(&self) -> bool {
		self.nodes.iter().any(|entry| {
			let node = entry.value();
			node.connection_type == ConnectionType::Incoming && node.state.is_connected()
		})
	}

	/// Periodically announces our own address to all connected peers
	///
	/// Only runs if we have incoming peers (proving our port is reachable)
	/// and a consensus external IP from version messages
	pub async fn run_self_announce(self: Arc<Self>) {
		loop {
			tokio::time::sleep(SELF_ANNOUNCE_INTERVAL).await;

			if !self.has_incoming_connected() {
				debug!("Self-announce skipped: no incoming peers connected");
				continue;
			}

			let Some(external_ip) = self.get_external_ip() else {
				debug!("Self-announce skipped: no consensus on external IP");
				continue;
			};

			let addr = NetworkAddress::new(external_ip, DEFAULT_PORT);
			let msg = MessageAddr::new(vec![addr]);
			let payload = msg.to_bytes();

			// Collect writers to avoid holding DashMap locks across awaits
			let writers: Vec<(IpAddr, SharedTcpWriter)> = self
				.nodes
				.iter()
				.filter_map(|entry| {
					if let NodeState::Connected { ref writer } = entry.value().state {
						Some((*entry.key(), Arc::clone(writer)))
					} else {
						None
					}
				})
				.collect();

			if writers.is_empty() {
				continue;
			}

			info!(
				"Announcing own address {}:{} to {} peers",
				external_ip,
				DEFAULT_PORT,
				writers.len()
			);

			for (address, writer) in &writers {
				if let Err(e) = writer.send_message("addr", &payload).await {
					debug!("Failed to self-announce to {}: {}", address, e);
				}
			}
		}
	}

	/// Collects outgoing peers with activity for persistence
	pub fn collect_peers_for_save(&self) -> crate::storage::peers::PeerDb {
		let peers = self
			.nodes
			.iter()
			.filter(|entry| {
				let node = entry.value();
				// Only save outgoing nodes that completed a handshake this session.
				// node.version is set when a version message is received and never
				// reset on reconnection, unlike version_received which toggles
				node.connection_type == ConnectionType::Outgoing && node.version > 0 && node.last_seen > 0
			})
			.map(|entry| {
				let node = entry.value();
				crate::storage::peers::SavedPeer {
					ip: *entry.key(),
					port: node.port,
					services: node.services.bits(),
					last_seen: node.last_seen,
					user_agent: node.user_agent.clone(),
					height: node.height,
				}
			})
			.collect();

		crate::storage::peers::PeerDb { version: 1, peers }
	}

	/// Collects active bans for persistence
	pub fn collect_bans_for_save(&self) -> crate::storage::bans::BanDb {
		let now = unix_now();
		let bans = self
			.nodes
			.iter()
			.filter_map(|entry| {
				if let NodeState::Banned {
					reason,
					created,
					expires,
				} = &entry.value().state
				{
					if *expires > now {
						return Some(crate::storage::bans::SavedBan {
							ip: *entry.key(),
							reason: reason.to_string(),
							created: *created,
							expires: *expires,
						});
					}
				}
				None
			})
			.collect();

		crate::storage::bans::BanDb { version: 1, bans }
	}

	/// Loads saved peers and connects them immediately
	///
	/// Skips loading if the db version is unrecognized. Uses `insert_outgoing`
	/// to spawn connection tasks so peers connect right away on startup
	pub fn load_saved_peers(self: &Arc<Self>, db: &crate::storage::peers::PeerDb) {
		if db.version != 1 {
			warn!(version = db.version, "Unknown peers.dat version, skipping");
			return;
		}
		for peer in &db.peers {
			// Populate metadata before connecting so we retain peer info from last session
			if self.insert(peer.ip, peer.port, ConnectionType::Outgoing) {
				if let Some(mut node) = self.nodes.get_mut(&peer.ip) {
					node.services = ServiceMask::from_bits_truncate(peer.services);
					node.last_seen = peer.last_seen;
					node.user_agent.clone_from(&peer.user_agent);
					node.height = peer.height;
				}
				// Spawn connection task
				let nm = Arc::clone(self);
				let addr = peer.ip;
				let port = peer.port;
				tokio::spawn(async move {
					handle_node_connection(nm, addr, port, 0).await;
				});
			}
		}
	}

	/// Loads saved bans and applies them
	///
	/// Skips loading if the db version is unrecognized
	pub fn load_saved_bans(&self, db: &crate::storage::bans::BanDb) {
		if db.version != 1 {
			warn!(version = db.version, "Unknown banlist.dat version, skipping");
			return;
		}
		let now = unix_now();
		for ban in &db.bans {
			if ban.expires <= now {
				continue;
			}
			// Port is irrelevant for banned nodes -- gets updated if the node
			// is later revived by an addr message
			self.insert(ban.ip, 0, ConnectionType::Outgoing);
			if let Some(mut node) = self.nodes.get_mut(&ban.ip) {
				node.state = NodeState::Banned {
					reason: match ban.reason.as_str() {
						"protocol violation" => BanReason::ProtocolViolation,
						"misbehavior" => BanReason::Misbehavior,
						other => {
							warn!(reason = other, ip = %ban.ip, "Unknown ban reason, defaulting to ProtocolViolation");
							BanReason::ProtocolViolation
						}
					},
					created: ban.created,
					expires: ban.expires,
				};
			}
		}
	}
}

/// Schedules a retry or marks a node as dead based on attempt count
fn schedule_retry(node_manager: &NodeManager, address: &IpAddr, attempt: u32) {
	let next_attempt = attempt.saturating_add(1);
	if next_attempt >= MAX_RETRY_ATTEMPTS {
		info!("Node {} exhausted all retry attempts, marking dead", address);
		node_manager.set_state(address, NodeState::Dead);
	} else {
		let backoff = calculate_backoff(next_attempt);
		debug!("Node {} scheduling retry {} in {:?}", address, next_attempt, backoff);
		// Instant::now() + bounded Duration cannot overflow in practice
		#[allow(clippy::arithmetic_side_effects)]
		let retry_at = Instant::now() + backoff;
		node_manager.set_state(
			address,
			NodeState::Disconnected {
				retry_at,
				attempt: next_attempt,
			},
		);
	}
}

/// Handles the connection to a node
async fn handle_node_connection(node_manager: Arc<NodeManager>, address: IpAddr, port: u16, attempt: u32) {
	if !node_manager.should_attempt_connection(&address) {
		trace!(
			"Avoiding connection to node {}:{} as it is not a good candidate",
			address,
			port
		);
		return;
	}

	// Attempt to establish a TCP connection with retries
	let mut tcp_stream = None;

	for tcp_attempt in 1..=TCP_MAX_ATTEMPTS {
		let fail_reason = match timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((address, port))).await {
			Ok(Ok(stream)) => {
				info!("Connected to {}:{} on attempt {}", address, port, tcp_attempt);
				tcp_stream = Some(stream);
				break;
			}
			Ok(Err(e)) => format!("{e}"),
			Err(_) => "timed out".to_string(),
		};

		if tcp_attempt < TCP_MAX_ATTEMPTS {
			trace!(
				"Failed to connect to {}:{} on attempt {}: {}",
				address,
				port,
				tcp_attempt,
				fail_reason
			);
			sleep(TCP_RETRY_DELAY).await;
		} else {
			debug!(
				"Failed to connect to {}:{} after {} attempts: {}",
				address, port, TCP_MAX_ATTEMPTS, fail_reason
			);
			schedule_retry(&node_manager, &address, attempt);
			return;
		}
	}

	let Some(mut tcp_stream) = tcp_stream else {
		error!(
			"Failed to establish connection to {}:{} after {} attempts",
			address, port, TCP_MAX_ATTEMPTS
		);
		return;
	};

	// TCP connected, transition to handshaking
	// Reset handshake fields so a reconnected node doesn't carry stale state
	if let Some(mut node) = node_manager.nodes.get_mut(&address) {
		node.state = NodeState::Handshaking { since: Instant::now() };
		node.version_received = false;
	}

	// Once the connection is established, the first step is to present ourselves
	// by sending a MessageVersion packet

	let network_address = NetworkAddress::new(address, port);
	let version = MessageVersion::new(network_address, node_manager.my_nonce);
	let packet = match Message::new("version", &version.to_bytes()) {
		Ok(p) => p,
		Err(e) => {
			error!("Failed to create version message for {}:{}: {}", address, port, e);
			schedule_retry(&node_manager, &address, attempt);
			return;
		}
	};

	if let Err(e) = tcp_stream.write_all(&packet.to_bytes()).await {
		error!("Failed to send version to {}:{}: {}", address, port, e);
		schedule_retry(&node_manager, &address, attempt);
		return;
	}

	// Hand off the connection to the connection loop
	node_connection_loop(Arc::clone(&node_manager), address, tcp_stream).await;

	// Don't overwrite banned nodes -- they were banned for a reason
	if let Some(node) = node_manager.nodes.get(&address) {
		if node.state.is_banned() {
			debug!("Node {} is banned, not scheduling retry", address);
			return;
		}
	}

	// Connection loop ended -- schedule retry
	schedule_retry(&node_manager, &address, attempt);

	debug!("Exiting handle_node_connection for {}:{}", address, port);
}

/// Main read loop for a node connection
// Explicit pub(crate) signals intent: only network.rs should call this directly
#[allow(clippy::redundant_pub_crate)]
pub(crate) async fn node_connection_loop(node_manager: Arc<NodeManager>, address: IpAddr, tcp_stream: TcpStream) {
	// Split the TCP stream into separate reader and writer
	let (mut tcp_reader, tcp_writer) = tcp_stream.into_split();

	// Wrap the writer on Arc<Mutex> so we can write from multiple places later on
	let shared_writer: SharedTcpWriter = Arc::new(Mutex::new(tcp_writer));

	let mut ping_interval = tokio::time::interval(PING_INTERVAL);
	// The first tick completes immediately; consume it so we don't ping on connect
	ping_interval.tick().await;

	let mut incoming_queue = NetworkQueue::new();
	let mut buf = [0; 8192];
	let mut last_activity = Instant::now();

	'main: loop {
		// Compute idle deadline outside select! to avoid attribute issues on arms
		// Instant::now() + bounded Duration cannot overflow in practice
		#[allow(clippy::arithmetic_side_effects)]
		let idle_deadline = last_activity + IDLE_TIMEOUT;

		tokio::select! {
			result = io::AsyncReadExt::read(&mut tcp_reader, &mut buf) => {
				match result {
					Ok(0) => break, // Connection closed
					Ok(n) => {
						last_activity = Instant::now();

						// Process the incoming data
						// n is bounded by buf.len() since it comes from read()
						#[allow(clippy::indexing_slicing)]
						if let Err(e) = incoming_queue.process_incoming_data(&buf[..n]) {
							warn!("Error processing data from {}: {}", address, e);
							break;
						}
					}
					Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
						continue; // No data available, try again
					}
					Err(e) => {
						warn!("Error in try_read(): {:?}", e);
						break;
					}
				}
			}
			_ = ping_interval.tick() => {
				node_manager.send_ping(&address).await;
			}
			() = tokio::time::sleep_until(idle_deadline) => {
				warn!("Connection to {} idle for {:?}, disconnecting", address, IDLE_TIMEOUT);
				break;
			}
		}

		// Process all messages in the queue
		while let Some(message) = incoming_queue.get_next_message() {
			node_manager.update_last_seen(&address);

			if let Err(e) = parse_incoming_message(&node_manager, &address, &shared_writer, message).await {
				warn!("Error parsing message: {:?}", e);

				// Close the connection
				if let Err(e) = shared_writer.lock().await.shutdown().await {
					warn!("Error shutting down writer for {}: {}", address, e);
				}
				break 'main;
			}
		}
	}
}

/// Parses and dispatches an incoming message to the appropriate handler
async fn parse_incoming_message(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	message: Message,
) -> Result<()> {
	let command = if let Ok(str) = CStr::from_bytes_until_nul(&message.command) {
		str.to_str()
			.map_err(|e| anyhow!("command field is not valid UTF-8: {e}"))?
	} else {
		warn!("Malformed 'command' field in message from {}", address);
		return Err(anyhow!("Error parsing incoming message, 'command' field is malformed"));
	};

	let command = NetworkCommand::from_command_str(command);

	debug!("Received message: {:?} from {}", command, address);

	// Only allow Version/Verack before the handshake completes
	if !matches!(command, NetworkCommand::Version | NetworkCommand::Verack) {
		let is_connected = node_manager.nodes.get(address).is_some_and(|n| n.state.is_connected());
		if !is_connected {
			warn!(
				"Received {:?} from {} before handshake complete, ignoring",
				command, address
			);
			return Ok(());
		}
	}

	match command {
		NetworkCommand::Version => handle_version(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::Verack => handle_verack(node_manager, address, tcp_writer).await,
		NetworkCommand::Ping => handle_ping(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::Pong => {
			handle_pong(node_manager, address, &message.payload);
			Ok(())
		}
		NetworkCommand::Addr => handle_addr(node_manager, address, &message.payload).await,
		NetworkCommand::Alert => {
			debug!("Received alert from {}, ignoring", address);
			Ok(())
		}
		NetworkCommand::GetAddr => handle_getaddr(node_manager, address, tcp_writer).await,
		NetworkCommand::Inv => handle_inv(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::GetData => handle_getdata(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::NotFound => {
			handle_notfound(address, &message.payload);
			Ok(())
		}
		NetworkCommand::Headers => handle_headers(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::GetHeaders => handle_getheaders(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::SendHeaders => {
			handle_sendheaders(node_manager, address);
			Ok(())
		}
		NetworkCommand::Unknown(cmd) => {
			warn!("Unknown command from {}: {}", address, cmd);
			Ok(())
		}
	}
}

/// Handles an incoming version message from a peer
async fn handle_version(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let version = MessageVersion::from_bytes(payload).context("failed to parse version message")?;

	// Phase 1: Validate under lock, then release
	let (is_incoming, port) = {
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager"))?;

		// Nodes can send only one version command
		if node.version_received {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Node {} sent version command twice, banning", address);
			return Err(anyhow!("Node {address} sent version command twice"));
		}

		if version.nonce == node_manager.my_nonce {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Self-connection detected to {}, banning", address);
			return Err(anyhow!("Node {address} is myself"));
		}

		(node.connection_type == ConnectionType::Incoming, node.port)
		// RefMut dropped here
	};

	// Phase 2: Async sends without holding any lock
	if is_incoming {
		let network_address = NetworkAddress::new(*address, port);
		let version_message = MessageVersion::new(network_address, node_manager.my_nonce);
		tcp_writer
			.send_message("version", &version_message.to_bytes())
			.await
			.context("failed to send version reply for inbound connection")?;
	}

	// Validate start_height
	if version.start_height < 0 {
		if let Some(mut node) = node_manager.nodes.get_mut(address) {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::Misbehavior,
				created: now,
				expires: ban_expires_at(now),
			};
		}
		warn!(
			"Node {} sent negative start_height {}, banning",
			address, version.start_height
		);
		return Err(anyhow!("Node {address} sent negative start_height"));
	}

	// Warn on extreme clock skew but don't ban (clock drift is common)
	// Protocol uses i64 for timestamp; u64 seconds won't wrap for ~584 billion years
	// i64 subtraction of two timestamps can't overflow in practice (both near current epoch)
	#[allow(clippy::cast_possible_wrap, clippy::arithmetic_side_effects)]
	let time_diff = (version.timestamp - unix_now() as i64).unsigned_abs();
	if time_diff > 4200 {
		warn!("Node {} clock skew is {}s (threshold 4200s)", address, time_diff);
	}

	// Phase 3: Re-acquire lock to write fields
	// Verify state hasn't changed between phases (another task could have banned the node)
	{
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager during version handling"))?;

		if !matches!(node.state, NodeState::Handshaking { .. }) {
			return Err(anyhow!(
				"node {address} state changed during version handling (now {})",
				node.state
			));
		}

		node.services = version.services;
		node.timestamp = version.timestamp;
		node.user_agent = sanitize_user_agent(&version.user_agent);
		node.height = version.start_height;
		node.version = version.version;
		node.version_received = true;
		node.relay = version.relay;
		node.pending_external_ip = Some(version.addr_recv.address);
	}

	// Verack sent after all locks released
	tcp_writer
		.send_message("verack", &[])
		.await
		.context("failed to send verack")
}

/// Handles an incoming verack message from a peer
async fn handle_verack(node_manager: &Arc<NodeManager>, address: &IpAddr, tcp_writer: &SharedTcpWriter) -> Result<()> {
	let pending_ip = {
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager"))?;

		// Verack before version is a protocol violation
		if !node.version_received {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Node {} sent verack before version, banning", address);
			return Err(anyhow!("Node {address} sent verack before version"));
		}

		info!(
			"Connection ready with node {} version={}, blocks={}, user_agent={}",
			address, node.version, node.height, node.user_agent
		);

		node.state = NodeState::Connected {
			writer: Arc::clone(tcp_writer),
		};
		node.sent_getaddr = true;

		node.pending_external_ip.take()
		// RefMut dropped here
	};

	// Record external IP vote only after handshake completes
	if let Some(ip) = pending_ip {
		node_manager.record_external_ip_vote(ip);
	}

	tcp_writer
		.send_message("getaddr", &[])
		.await
		.context("failed to send getaddr")?;

	// Send sendheaders if peer supports it (BIP 130)
	let peer_version = node_manager.nodes.get(address).map_or(0, |n| n.version);
	if peer_version >= SENDHEADERS_VERSION {
		tcp_writer
			.send_message("sendheaders", &[])
			.await
			.context("failed to send sendheaders")?;
	}

	// Begin header sync
	let locator = node_manager.header_store.read().build_locator();
	let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
	tcp_writer
		.send_message("getheaders", &getheaders.to_bytes())
		.await
		.context("failed to send initial getheaders")?;

	debug!(peer = %address, "sent initial getheaders for header sync");

	Ok(())
}

/// Handles an incoming ping message from a peer
async fn handle_ping(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	// Pre-BIP31 nodes send 0-byte pings; just acknowledge silently
	if payload.is_empty() {
		debug!("Received pre-BIP31 ping (no nonce) from {}", address);
		return Ok(());
	}

	if payload.len() != 8 {
		warn!("Received malformed ping command from {}", address);

		let now = unix_now();
		node_manager.set_state(
			address,
			NodeState::Banned {
				reason: BanReason::Misbehavior,
				created: now,
				expires: ban_expires_at(now),
			},
		);

		return Err(anyhow!("Received malformed ping command from {address}"));
	}

	let nonce = vec_to_u64_le(payload).context("failed to parse ping nonce")?;
	debug!("Received ping command from {} with nonce {}", address, nonce);

	tcp_writer
		.send_message("pong", &nonce.to_le_bytes())
		.await
		.context("failed to send pong")
}

/// Handles an incoming pong message from a peer
///
/// Validates that the nonce matches the last ping we sent
fn handle_pong(node_manager: &NodeManager, address: &IpAddr, payload: &[u8]) {
	if payload.len() != 8 {
		warn!(
			"Received malformed pong from {} ({} bytes, expected 8)",
			address,
			payload.len()
		);
		return;
	}

	let Ok(nonce) = vec_to_u64_le(payload) else {
		warn!("Failed to parse pong nonce from {}", address);
		return;
	};

	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		match node.last_ping_nonce.take() {
			Some(expected) if expected == nonce => {
				debug!("Valid pong from {}", address);
			}
			Some(expected) => {
				warn!(
					"Pong nonce mismatch from {}: expected {}, got {}",
					address, expected, nonce
				);
			}
			None => {
				debug!("Received unsolicited pong from {}", address);
			}
		}
	}
}

/// Strips non-printable and non-ASCII characters from a user agent string
///
/// Prevents terminal escape sequence injection via malicious user agents.
/// Trims leading/trailing whitespace to prevent display confusion
fn sanitize_user_agent(s: &str) -> String {
	s.chars()
		.filter(|c| c.is_ascii_graphic() || *c == ' ')
		.take(MAX_USER_AGENT_DISPLAY)
		.collect::<String>()
		.trim()
		.to_string()
}

/// Computes a deterministic score for relay peer selection
///
/// Uses `SipHash-1-3` keyed by `relay_key` to produce a stable ranking of peers
/// for a given address within a 24-hour time bucket. Same inputs always produce same output
fn compute_relay_score(relay_key: u64, addr_hash: u64, time_bucket: u64, peer_hash: u64) -> u64 {
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
fn refill_addr_tokens(tokens: f64, elapsed: Duration) -> f64 {
	// Both operands are finite and bounded; result is capped by min()
	#[allow(clippy::arithmetic_side_effects, clippy::float_arithmetic)]
	elapsed
		.as_secs_f64()
		.mul_add(ADDR_TOKEN_RATE, tokens)
		.min(ADDR_TOKEN_CAPACITY)
}

/// Handles an incoming addr message containing peer addresses
async fn handle_addr(node_manager: &Arc<NodeManager>, address: &IpAddr, payload: &[u8]) -> Result<()> {
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
async fn handle_getaddr(node_manager: &Arc<NodeManager>, address: &IpAddr, tcp_writer: &SharedTcpWriter) -> Result<()> {
	// Rate limit getaddr responses to 1 per minute
	{
		let Some(mut node) = node_manager.nodes.get_mut(address) else {
			return Ok(());
		};

		if let Some(last) = node.last_getaddr_response {
			if last.elapsed() < Duration::from_secs(60) {
				debug!("Rate limiting getaddr from {}", address);
				return Ok(());
			}
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

/// Handles an incoming inv message from a peer
async fn handle_inv(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let inv = MessageInv::from_bytes(payload).context("failed to parse inv message")?;

	debug!(count = inv.items().len(), peer = %address, "received inv");

	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		if node.inv_known.len() >= MAX_INV_KNOWN {
			node.inv_known.clear();
		}

		for item in inv.items() {
			node.inv_known.insert(item.hash);
		}
	}

	// Request headers if any announced blocks are unknown to us
	let has_new_blocks = {
		let store = node_manager.header_store.read();
		inv.items()
			.iter()
			.any(|item| item.inv_type == InvType::Block && store.get(&item.hash).is_none())
	};

	if has_new_blocks {
		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send getheaders after block inv")?;
		debug!(peer = %address, "requesting headers after block inv");
	}

	Ok(())
}

/// Handles an incoming getdata message from a peer
///
/// Responds with notfound for all items since we have no data to serve yet.
/// Echoes the raw payload back as notfound -- the wire format is identical,
/// and this preserves items with unknown inv types that `from_bytes` would drop
async fn handle_getdata(
	_node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	debug!(peer = %address, payload_len = payload.len(), "received getdata, responding with notfound");

	// Echo the payload verbatim: inv/getdata/notfound share the same wire format,
	// and we have nothing to serve, so every requested item is "not found"
	tcp_writer
		.send_message("notfound", payload)
		.await
		.context("failed to send notfound")
}

/// Handles an incoming notfound message from a peer
fn handle_notfound(address: &IpAddr, payload: &[u8]) {
	match MessageInv::from_bytes(payload) {
		Ok(inv) => {
			debug!(count = inv.items().len(), peer = %address, "received notfound");
		}
		Err(e) => {
			warn!(peer = %address, error = %e, "failed to parse notfound message");
		}
	}
}

/// Handles an incoming sendheaders message (BIP 130)
///
/// Sets the peer's preference flag so we send future block
/// announcements via headers instead of inv
fn handle_sendheaders(node_manager: &NodeManager, address: &IpAddr) {
	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		node.prefer_headers = true;
		debug!("Peer {} prefers headers announcements", address);
	}
}

/// Handles an incoming headers message from a peer
///
/// Validates chain continuity via batch insertion, then requests
/// more headers if we received a full batch (2000)
async fn handle_headers(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageHeaders::from_bytes(payload).context("failed to parse headers message")?;
	let count = msg.headers().len();

	if count == 0 {
		return Ok(());
	}

	let headers = msg.into_headers();

	// Batch insert under a single write lock. Don't disconnect on failure --
	// the peer may have sent orphan or fork headers that we can't connect yet.
	// Log the error and move on instead of killing the connection
	let insert_result = node_manager.header_store.write().add_headers(&headers);
	let added = match insert_result {
		Ok(n) => n,
		Err(e) => {
			warn!(peer = %address, error = %e, "failed to add headers, ignoring batch");
			return Ok(());
		}
	};

	info!(received = count, added, peer = %address, "processed headers");

	// If we got a full batch, request more
	if count == MAX_HEADERS_PER_MSG {
		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send follow-up getheaders")?;
		debug!(peer = %address, "requesting more headers");
	}

	Ok(())
}

/// Handles an incoming getheaders request from a peer
///
/// Finds the fork point using the locator hashes, then sends
/// up to 2000 headers from our chain via the height index
async fn handle_getheaders(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageGetHeaders::from_bytes(payload).context("failed to parse getheaders")?;

	// Scope the read lock so it drops before any await point
	let response_headers = {
		let store = node_manager.header_store.read();

		// Find fork point: first locator hash that exists in our chain
		let mut start_height: u32 = 0;
		for hash in msg.locator_hashes() {
			if *hash == store.genesis() {
				start_height = 0;
				break;
			}
			if let Some(stored) = store.get(hash) {
				start_height = stored.height;
				break;
			}
		}

		store.get_headers_after(start_height, MAX_HEADERS_PER_MSG, msg.hash_stop())
	};

	if response_headers.is_empty() {
		debug!(peer = %address, "no headers to send for getheaders");
		return Ok(());
	}

	debug!(count = response_headers.len(), peer = %address, "sending headers response");

	let response = MessageHeaders::new(response_headers);
	tcp_writer
		.send_message("headers", &response.to_bytes())
		.await
		.context("failed to send headers response")
}

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode.
// DashMap guard drop order doesn't matter in synchronous test functions
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::significant_drop_tightening)]
mod tests {
	use super::*;

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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "8.8.8.8".parse().unwrap();
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		assert_eq!(nm.get_external_ip(), Some(ip));
	}

	#[test]
	fn get_external_ip_requires_three_votes() {
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "8.8.8.8".parse().unwrap();
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		assert_eq!(nm.get_external_ip(), None);
	}

	#[test]
	fn get_external_ip_returns_majority() {
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "1.2.3.4".parse().unwrap();
		nm.insert(ip, 12345, ConnectionType::Incoming);

		// Node starts in Handshaking, not Connected -- should return false
		assert!(!nm.has_incoming_connected());
	}

	#[test]
	fn addr_known_dedup_prevents_duplicate_insert() {
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let node = nm.nodes.get(&ip).unwrap();
		assert!(!node.sent_getaddr);
	}

	#[test]
	fn sent_getaddr_can_be_set() {
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "10.0.0.1".parse().unwrap();

		assert!(nm.insert(ip, 9933, ConnectionType::Outgoing));
		assert!(!nm.insert(ip, 8888, ConnectionType::Outgoing));
		assert_eq!(nm.nodes.len(), 1);
	}

	#[test]
	fn insert_updates_port_when_disconnected() {
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		// Create a real TCP pair to get an OwnedWriteHalf
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let stream = TcpStream::connect(addr).await.unwrap();
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let stream = TcpStream::connect(addr).await.unwrap();
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = Arc::new(NodeManager::new(test_genesis()));
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = Arc::new(NodeManager::new(test_genesis()));
		let db = crate::storage::peers::PeerDb {
			version: 999,
			peers: vec![crate::storage::peers::SavedPeer {
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
		let nm = NodeManager::new(test_genesis());
		let db = crate::storage::bans::BanDb {
			version: 999,
			bans: vec![crate::storage::bans::SavedBan {
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
		let nm = NodeManager::new(test_genesis());
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
		let nm = NodeManager::new(test_genesis());
		let ip: IpAddr = "10.0.0.1".parse().unwrap();

		let guard = nm.insert_incoming(ip, 54321);
		assert!(guard.is_some());
		assert_eq!(nm.incoming_count.load(Ordering::Acquire), 1);

		drop(guard);
		assert_eq!(nm.incoming_count.load(Ordering::Acquire), 0);
	}
}
