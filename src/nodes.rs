// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use rand::RngCore;
use std::{collections::HashSet, ffi::CStr, fmt, net::IpAddr, sync::Arc, time::Duration};
use tokio::{
	io::{self, AsyncWriteExt, BufReader},
	net::TcpStream,
	sync::Mutex,
	time::{sleep, timeout, Instant},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
	network::{
		message_addr::{AddrEntry, MessageAddr},
		message_version::MessageVersion,
		Message, NetworkAddress, NetworkCommand, NetworkQueue, ServiceMask, SharedTcpWriter, SharedTcpWriterExt,
	},
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

/// How often to announce our own address to peers
const SELF_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Reason why a node was permanently banned
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
	/// Permanently banned -- never retry
	Banned { reason: BanReason },
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

	/// Whether this node is permanently banned
	pub const fn is_banned(&self) -> bool {
		matches!(self, Self::Banned { .. })
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
			Self::Banned { reason } => write!(f, "Banned({reason})"),
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

/// Represents a unique identifier for a node in the network
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct NodeEndpoint {
	pub address: IpAddr,
	pub port: u16,
}

impl fmt::Display for NodeEndpoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}:{}", self.address, self.port)
	}
}

/// Summary statistics about managed nodes
#[derive(Debug)]
pub struct NodeStats {
	pub total: usize,
	pub connected: usize,
	pub disconnected: usize,
}

/// Direction of a node connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
	Incoming,
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

/// Lightweight snapshot of node data for display purposes
///
/// Contains only the fields needed by the TUI, without internal state
/// like `tcp_writer` or the full `NodeState` enum
pub struct NodeSnapshot {
	pub endpoint: NodeEndpoint,
	pub height: i32,
	pub connection_type: ConnectionType,
	pub state_label: String,
}

/// Represents a node in the Catcoin network
pub struct Node {
	pub endpoint: NodeEndpoint,

	/// Unix timestamp
	pub last_seen: u64,

	pub version: u32,

	/// Whether we have received a version message from this peer
	pub version_received: bool,

	pub services: ServiceMask,

	pub timestamp: i64,

	pub user_agent: String,

	pub height: i32,

	pub relay: bool,

	pub connection_type: ConnectionType,

	/// Current connection lifecycle state
	pub state: NodeState,

	/// Addresses already relayed to this peer in the current 24h window
	pub addr_known: HashSet<(IpAddr, u16)>,

	/// The 24h time bucket when `addr_known` was last cleared
	pub addr_known_bucket: u64,

	/// Token bucket: available tokens for rate limiting incoming addr entries
	pub addr_tokens: f64,

	/// Token bucket: last time tokens were refilled
	pub last_token_refill: Instant,

	/// Whether we sent getaddr to this peer (inhibits relay of their addr response)
	pub sent_getaddr: bool,
}

/// Manages a collection of nodes in the network
pub struct NodeManager {
	// Using DashMap for concurrent access without needing explicit locking
	pub(crate) nodes: DashMap<NodeEndpoint, Node>,

	// Used to avoid connecting to myself
	pub my_nonce: u64,

	/// Per-session random key for deterministic addr relay peer selection
	pub(crate) relay_key: u64,

	/// Peer votes for our external IP address
	external_ip_votes: DashMap<IpAddr, u32>,
}

impl Default for NodeManager {
	fn default() -> Self {
		Self::new()
	}
}

impl NodeManager {
	/// Creates a new `NodeManager` with an empty node set and random nonce and relay key
	pub fn new() -> Self {
		let mut rng = rand::thread_rng();
		Self {
			nodes: DashMap::new(),
			my_nonce: rng.next_u64(),
			relay_key: rng.next_u64(),
			external_ip_votes: DashMap::new(),
		}
	}

	/// Inserts a new node into the manager if it doesn't already exist
	///
	/// Returns true if the node was inserted, false if it already existed
	pub fn insert(&self, address: IpAddr, port: u16, connection_type: ConnectionType) -> bool {
		if self.nodes.len() >= MAX_NODES {
			debug!("Node limit reached ({}), not adding {}:{}", MAX_NODES, address, port);
			return false;
		}

		let endpoint = NodeEndpoint { address, port };

		// Use entry API for atomic check-and-insert
		let entry = self.nodes.entry(endpoint.clone());

		match entry {
			dashmap::mapref::entry::Entry::Occupied(_) => false,
			dashmap::mapref::entry::Entry::Vacant(vacant) => {
				let initial_state = match connection_type {
					ConnectionType::Outgoing => NodeState::Connecting { since: Instant::now() },
					ConnectionType::Incoming => NodeState::Handshaking { since: Instant::now() },
				};

				let node = Node {
					endpoint,
					last_seen: 0,
					height: 0,
					relay: false,
					services: ServiceMask::empty(),
					timestamp: 0,
					user_agent: String::new(),
					version: 0,
					version_received: false,
					connection_type,
					state: initial_state,
					addr_known: HashSet::new(),
					addr_known_bucket: 0,
					addr_tokens: ADDR_TOKEN_CAPACITY,
					last_token_refill: Instant::now(),
					sent_getaddr: false,
				};

				vacant.insert(node);
				true
			}
		}
	}

	/// Updates the `last_seen` timestamp for a node with the current time
	pub fn update_last_seen(&self, node_endpoint: &NodeEndpoint) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.last_seen = unix_now();
		}
	}

	/// Updates the connection state of a node
	pub fn set_state(&self, node_endpoint: &NodeEndpoint, state: NodeState) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.state = state;
		}
	}

	/// Checks if the node is appropriate to try a connection
	pub fn is_candidate(&self, node_endpoint: &NodeEndpoint) -> bool {
		if let Some(node) = self.nodes.get(node_endpoint) {
			return matches!(node.state, NodeState::Connecting { .. }) || node.state.is_connectable();
		}

		false
	}

	/// Sends a ping message to a connected node
	pub async fn send_ping(&self, node_endpoint: &NodeEndpoint) {
		let tcp_writer = if let Some(node) = self.nodes.get(node_endpoint) {
			if let NodeState::Connected { ref writer } = node.state {
				Arc::clone(writer)
			} else {
				debug!("Skipping ping to {}, not connected", node_endpoint);
				return;
			}
		} else {
			debug!("Skipping ping to {}, node not found", node_endpoint);
			return;
		};

		debug!("Sending ping to {}", node_endpoint);

		let nonce = rand::thread_rng().next_u64();

		if let Err(e) = tcp_writer.send_message("ping", &nonce.to_le_bytes()).await {
			warn!("Failed to send ping to {}: {}", node_endpoint, e);
		}
	}

	/// Returns summary statistics about the managed nodes
	pub fn get_stats(&self) -> NodeStats {
		let total = self.nodes.len();
		let connected = self.nodes.iter().filter(|n| n.state.is_connected()).count();
		let disconnected = total.saturating_sub(connected);

		NodeStats {
			total,
			connected,
			disconnected,
		}
	}

	/// Returns snapshots of all nodes for display in the TUI
	pub fn get_all_nodes(&self) -> Vec<NodeSnapshot> {
		self.nodes
			.iter()
			.map(|entry| {
				let node = entry.value();
				NodeSnapshot {
					endpoint: node.endpoint.clone(),
					height: node.height,
					connection_type: node.connection_type,
					state_label: node.state.to_string(),
				}
			})
			.collect()
	}

	/// Background task that periodically scans for disconnected nodes ready for retry
	///
	/// Runs every 30 seconds and spawns connection tasks for nodes whose
	/// backoff period has elapsed
	pub async fn run_reaper(self: Arc<Self>) {
		loop {
			tokio::time::sleep(REAPER_SCAN_INTERVAL).await;

			// Collect endpoints of nodes ready for retry
			// We collect first to avoid holding DashMap locks during spawning
			let ready_nodes: Vec<(NodeEndpoint, u32)> = self
				.nodes
				.iter()
				.filter_map(|entry| {
					let node = entry.value();
					if let NodeState::Disconnected { attempt, .. } = &node.state {
						if node.state.is_connectable() {
							return Some((entry.key().clone(), *attempt));
						}
					}
					None
				})
				.collect();

			if !ready_nodes.is_empty() {
				info!("Reaper: {} nodes ready for retry", ready_nodes.len());
			}

			for (endpoint, attempt) in ready_nodes {
				// Transition to Connecting before spawning
				if let Some(mut node) = self.nodes.get_mut(&endpoint) {
					// Double-check state hasn't changed since we collected
					if !node.state.is_connectable() {
						continue;
					}
					node.state = NodeState::Connecting { since: Instant::now() };
				} else {
					continue;
				}

				let nm = Arc::clone(&self);
				let addr = endpoint.address;
				let port = endpoint.port;
				tokio::spawn(async move {
					handle_node_connection(nm, addr, port, attempt).await;
				});
			}

			// Detect nodes stuck in Connecting or Handshaking for too long
			let stale_nodes: Vec<NodeEndpoint> = self
				.nodes
				.iter()
				.filter_map(|entry| {
					let is_stale = match &entry.value().state {
						NodeState::Connecting { since } => since.elapsed() > STALE_CONNECTING_TIMEOUT,
						NodeState::Handshaking { since } => since.elapsed() > STALE_HANDSHAKE_TIMEOUT,
						_ => false,
					};
					if is_stale {
						Some(entry.key().clone())
					} else {
						None
					}
				})
				.collect();

			for endpoint in stale_nodes {
				warn!("Node {} stuck in stale state, scheduling retry", endpoint);
				if let Some(mut node) = self.nodes.get_mut(&endpoint) {
					if matches!(node.state, NodeState::Connecting { .. } | NodeState::Handshaking { .. }) {
						node.state = NodeState::Disconnected {
							retry_at: Instant::now(),
							attempt: 0,
						};
					}
				}
			}

			let stats = self.get_stats();
			debug!(
				"Reaper scan complete: {} total, {} connected, {} disconnected",
				stats.total, stats.connected, stats.disconnected
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

	/// Retrieves the `tcp_writer` for a connected node
	///
	/// Returns None if the node doesn't exist or isn't in Connected state
	// Part of the NodeManager encapsulation API, will replace direct field access
	#[allow(dead_code)]
	pub fn get_writer(&self, node_endpoint: &NodeEndpoint) -> Option<SharedTcpWriter> {
		if let Some(node) = self.nodes.get(node_endpoint) {
			if let NodeState::Connected { ref writer } = node.state {
				return Some(Arc::clone(writer));
			}
		}
		None
	}

	/// Shuts down all connected nodes cleanly by sending TCP FIN
	///
	/// Collects writers first to avoid holding `DashMap` locks across awaits
	pub async fn graceful_shutdown(&self) {
		let writers: Vec<(NodeEndpoint, SharedTcpWriter)> = self
			.nodes
			.iter()
			.filter_map(|entry| {
				if let NodeState::Connected { ref writer } = entry.state {
					Some((entry.endpoint.clone(), Arc::clone(writer)))
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

		for (endpoint, writer) in writers {
			if let Err(err) = writer.lock().await.shutdown().await {
				warn!(%endpoint, error = %err, "Failed to shut down writer");
			}
		}

		info!("Graceful shutdown complete");
	}

	/// Marks a node as permanently banned
	// Part of the NodeManager encapsulation API, will replace direct state mutation
	#[allow(dead_code)]
	pub fn ban_node(&self, node_endpoint: &NodeEndpoint, reason: BanReason) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.state = NodeState::Banned { reason };
		}
	}

	/// Records a peer's report of our external IP address
	///
	/// Non-routable IPs (private, loopback, link-local, etc) are silently ignored
	pub fn record_external_ip_vote(&self, ip: IpAddr) {
		if !is_routable(ip) {
			debug!(ip = %ip, "Ignoring non-routable external IP vote");
			return;
		}
		self.external_ip_votes
			.entry(ip)
			.and_modify(|count| {
				*count = count.saturating_add(1);
			})
			.or_insert(1);
	}

	/// Returns our external IP if at least 3 peers agree on it
	///
	/// Returns the IP with the most votes, or None if no IP has >= 3 votes
	pub fn get_external_ip(&self) -> Option<IpAddr> {
		self.external_ip_votes
			.iter()
			.filter(|entry| *entry.value() >= 3)
			.max_by_key(|entry| *entry.value())
			.map(|entry| *entry.key())
	}

	/// Revives a Dead node if the given timestamp is newer than `last_seen`
	///
	/// Returns true if the node was revived, false otherwise
	pub fn revive_if_newer(self: &Arc<Self>, endpoint: &NodeEndpoint, timestamp: u32) -> bool {
		if let Some(mut node) = self.nodes.get_mut(endpoint) {
			if matches!(node.state, NodeState::Dead) && u64::from(timestamp) > node.last_seen {
				node.state = NodeState::Connecting { since: Instant::now() };
				node.last_seen = u64::from(timestamp);
				drop(node);

				// Spawn a connection task for the revived node
				let nm = Arc::clone(self);
				let addr = endpoint.address;
				let port = endpoint.port;
				tokio::spawn(async move {
					handle_node_connection(nm, addr, port, 0).await;
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

			let addr = NetworkAddress::new(external_ip, 9933);
			let msg = MessageAddr::new(vec![addr]);
			let payload = msg.to_bytes();

			// Collect writers to avoid holding DashMap locks across awaits
			let writers: Vec<(NodeEndpoint, SharedTcpWriter)> = self
				.nodes
				.iter()
				.filter_map(|entry| {
					if let NodeState::Connected { ref writer } = entry.value().state {
						Some((entry.key().clone(), Arc::clone(writer)))
					} else {
						None
					}
				})
				.collect();

			if writers.is_empty() {
				continue;
			}

			info!("Announcing own address {}:9933 to {} peers", external_ip, writers.len());

			for (endpoint, writer) in &writers {
				if let Err(e) = writer.send_message("addr", &payload).await {
					debug!("Failed to self-announce to {}: {}", endpoint, e);
				}
			}
		}
	}
}

/// Schedules a retry or marks a node as dead based on attempt count
fn schedule_retry(node_manager: &NodeManager, endpoint: &NodeEndpoint, attempt: u32) {
	let next_attempt = attempt.saturating_add(1);
	if next_attempt >= MAX_RETRY_ATTEMPTS {
		info!("Node {} exhausted all retry attempts, marking dead", endpoint);
		node_manager.set_state(endpoint, NodeState::Dead);
	} else {
		let backoff = calculate_backoff(next_attempt);
		debug!("Node {} scheduling retry {} in {:?}", endpoint, next_attempt, backoff);
		// Instant::now() + bounded Duration cannot overflow in practice
		#[allow(clippy::arithmetic_side_effects)]
		let retry_at = Instant::now() + backoff;
		node_manager.set_state(
			endpoint,
			NodeState::Disconnected {
				retry_at,
				attempt: next_attempt,
			},
		);
	}
}

/// Handles the connection to a node
async fn handle_node_connection(node_manager: Arc<NodeManager>, address: IpAddr, port: u16, attempt: u32) {
	let node_endpoint = NodeEndpoint { address, port };

	if !node_manager.is_candidate(&node_endpoint) {
		trace!(
			"Avoiding connection to node {} as it is not a good candidate",
			&node_endpoint
		);
		return;
	}

	// Attempt to establish a TCP connection with retries
	let mut tcp_stream = None;

	for tcp_attempt in 1..=TCP_MAX_ATTEMPTS {
		match timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((address, port))).await {
			Ok(Ok(stream)) => {
				info!("Connected to {} on attempt {}", &node_endpoint, tcp_attempt);
				tcp_stream = Some(stream);
				break;
			}
			Ok(Err(e)) => {
				if tcp_attempt < TCP_MAX_ATTEMPTS {
					trace!(
						"Failed to connect to {} on attempt {}: {}",
						&node_endpoint,
						tcp_attempt,
						e
					);
					sleep(TCP_RETRY_DELAY).await;
				} else {
					debug!(
						"Failed to connect to {} after {} attempts: {}",
						&node_endpoint, TCP_MAX_ATTEMPTS, e
					);
					schedule_retry(&node_manager, &node_endpoint, attempt);
					return;
				}
			}
			Err(_) => {
				if tcp_attempt < TCP_MAX_ATTEMPTS {
					trace!("Connection to {} timed out on attempt {}", &node_endpoint, tcp_attempt);
					sleep(TCP_RETRY_DELAY).await;
				} else {
					debug!(
						"Connection to {} timed out after {} attempts",
						&node_endpoint, TCP_MAX_ATTEMPTS
					);
					schedule_retry(&node_manager, &node_endpoint, attempt);
					return;
				}
			}
		}
	}

	let Some(mut tcp_stream) = tcp_stream else {
		error!(
			"Failed to establish connection to {:?} after {} attempts",
			node_endpoint, TCP_MAX_ATTEMPTS
		);
		return;
	};

	// TCP connected, transition to handshaking
	// Reset handshake fields so a reconnected node doesn't carry stale state
	if let Some(mut node) = node_manager.nodes.get_mut(&node_endpoint) {
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
			error!("Failed to create version message for {}: {}", &node_endpoint, e);
			schedule_retry(&node_manager, &node_endpoint, attempt);
			return;
		}
	};

	if let Err(e) = tcp_stream.write_all(&packet.to_bytes()).await {
		error!("Failed to send version to {}: {}", &node_endpoint, e);
		schedule_retry(&node_manager, &node_endpoint, attempt);
		return;
	}

	// Hand off the connection to the connection loop
	node_connection_loop(Arc::clone(&node_manager), node_endpoint.clone(), tcp_stream).await;

	// Don't overwrite banned nodes -- they were banned for a reason
	if let Some(node) = node_manager.nodes.get(&node_endpoint) {
		if node.state.is_banned() {
			debug!("Node {} is banned, not scheduling retry", &node_endpoint);
			return;
		}
	}

	// Connection loop ended -- schedule retry
	schedule_retry(&node_manager, &node_endpoint, attempt);

	debug!("Exiting handle_node_connection for {:?}", node_endpoint);
}

/// Main read loop for a node connection
// Explicit pub(crate) signals intent: only network.rs should call this directly
#[allow(clippy::redundant_pub_crate)]
pub(crate) async fn node_connection_loop(
	node_manager: Arc<NodeManager>,
	node_endpoint: NodeEndpoint,
	tcp_stream: TcpStream,
) {
	// Split the TCP stream into separate reader and writer
	let (tcp_reader, tcp_writer) = tcp_stream.into_split();
	let mut tcp_reader = BufReader::with_capacity(8192, tcp_reader);

	// Wrap the writer on Arc<Mutex> so we can write from multiple places later on
	let shared_writer: SharedTcpWriter = Arc::new(Mutex::new(tcp_writer));

	let mut ping_interval = tokio::time::interval(PING_INTERVAL);
	// The first tick completes immediately; consume it so we don't ping on connect
	ping_interval.tick().await;

	let mut incoming_queue = NetworkQueue::new();

	'main: loop {
		let mut buf = [0; 4096];

		tokio::select! {
			result = io::AsyncReadExt::read(&mut tcp_reader, &mut buf) => {
				match result {
					Ok(0) => break, // Connection closed
					Ok(n) => {
						// Process the incoming data
						// n is bounded by buf.len() since it comes from read()
						#[allow(clippy::indexing_slicing)]
						if let Err(e) = incoming_queue.process_incoming_data(&buf[..n]) {
							warn!("Error processing data from {}: {}", &node_endpoint, e);
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
				node_manager.send_ping(&node_endpoint).await;
			}
		}

		// Process all messages in the queue
		while let Some(message) = incoming_queue.get_next_message() {
			node_manager.update_last_seen(&node_endpoint);

			if let Err(e) = parse_incoming_message(
				Arc::clone(&node_manager),
				&node_endpoint,
				Arc::clone(&shared_writer),
				message,
			)
			.await
			{
				warn!("Error parsing message: {:?}", e);

				// Close the connection
				if let Err(e) = shared_writer.lock().await.shutdown().await {
					warn!("Error shutting down writer for {}: {}", &node_endpoint, e);
				}
				break 'main;
			}
		}
	}
}

/// Parses and dispatches an incoming message to the appropriate handler
async fn parse_incoming_message(
	node_manager: Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: SharedTcpWriter,
	message: Message,
) -> Result<()> {
	let command = if let Ok(str) = CStr::from_bytes_until_nul(&message.command) {
		str.to_str()
			.map_err(|e| anyhow!("command field is not valid UTF-8: {e}"))?
	} else {
		warn!("Malformed 'command' field in message from {}", node_endpoint);
		return Err(anyhow!("Error parsing incoming message, 'command' field is malformed"));
	};

	let command = NetworkCommand::from_command_str(command);

	debug!("Received message: {:?} from {}", command, node_endpoint);

	match command {
		NetworkCommand::Version => handle_version(&node_manager, node_endpoint, &tcp_writer, &message.payload).await,
		NetworkCommand::Verack => handle_verack(&node_manager, node_endpoint, &tcp_writer).await,
		NetworkCommand::Ping => handle_ping(&node_manager, node_endpoint, &tcp_writer, &message.payload).await,
		NetworkCommand::Pong => {
			handle_pong(node_endpoint);
			Ok(())
		}
		NetworkCommand::Addr => handle_addr(&node_manager, node_endpoint, &message.payload).await,
		NetworkCommand::Alert => {
			debug!("Received alert from {}, ignoring", node_endpoint);
			Ok(())
		}
		NetworkCommand::GetAddr => handle_getaddr(&node_manager, node_endpoint, &tcp_writer).await,
		NetworkCommand::Unknown(cmd) => {
			warn!("Unknown command from {}: {}", node_endpoint, cmd);
			Ok(())
		}
	}
}

/// Handles an incoming version message from a peer
async fn handle_version(
	node_manager: &Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let version = MessageVersion::from_bytes(payload).context("failed to parse version message")?;

	// Phase 1: Validate under lock, then release
	let is_incoming = {
		let mut node = node_manager
			.nodes
			.get_mut(node_endpoint)
			.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager"))?;

		// Nodes can send only one version command
		if node.version_received {
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
			};
			warn!("Node {} sent version command twice, banning", node_endpoint);
			return Err(anyhow!("Node {node_endpoint} sent version command twice"));
		}

		if version.nonce == node_manager.my_nonce {
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
			};
			warn!("Self-connection detected to {}, banning", node_endpoint);
			return Err(anyhow!("Node {node_endpoint} is myself"));
		}

		node.connection_type == ConnectionType::Incoming
		// RefMut dropped here
	};

	// Phase 2: Async sends without holding any lock
	if is_incoming {
		let network_address = NetworkAddress::new(node_endpoint.address, node_endpoint.port);
		let version_message = MessageVersion::new(network_address, node_manager.my_nonce);
		tcp_writer
			.send_message("version", &version_message.to_bytes())
			.await
			.context("failed to send version reply for inbound connection")?;
	}

	// Phase 3: Re-acquire lock to write fields
	{
		let mut node = node_manager
			.nodes
			.get_mut(node_endpoint)
			.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager during version handling"))?;

		node.endpoint = node_endpoint.clone();
		node.services = version.services;
		node.timestamp = version.timestamp;
		node.user_agent = version.user_agent;
		node.height = version.start_height;
		node.version = version.version;
		node.version_received = true;
		node.relay = version.relay;
	}

	// Record peer's view of our external IP for consensus
	node_manager.record_external_ip_vote(version.addr_recv.address);

	// Verack sent after all locks released
	tcp_writer
		.send_message("verack", &[])
		.await
		.context("failed to send verack")
}

/// Handles an incoming verack message from a peer
async fn handle_verack(
	node_manager: &Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
	let mut node = node_manager
		.nodes
		.get_mut(node_endpoint)
		.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager"))?;

	// If we already received version from the peer, the handshake is complete
	if node.version_received {
		info!(
			"Connection ready with node {} version={}, blocks={}, user_agent={}",
			node_endpoint, node.version, node.height, node.user_agent
		);

		node.state = NodeState::Connected {
			writer: Arc::clone(tcp_writer),
		};

		drop(node);

		// Mark that we sent getaddr so we suppress relay of their addr response
		if let Some(mut node) = node_manager.nodes.get_mut(node_endpoint) {
			node.sent_getaddr = true;
		}

		tcp_writer
			.send_message("getaddr", &[])
			.await
			.context("failed to send getaddr")?;
	}
	// If version not received yet, stay in Handshaking state

	Ok(())
}

/// Handles an incoming ping message from a peer
async fn handle_ping(
	node_manager: &Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	// Pre-BIP31 nodes send 0-byte pings; just acknowledge silently
	if payload.is_empty() {
		debug!("Received pre-BIP31 ping (no nonce) from {}", node_endpoint);
		return Ok(());
	}

	if payload.len() != 8 {
		warn!("Received malformed ping command from {}", node_endpoint);

		node_manager.set_state(
			node_endpoint,
			NodeState::Banned {
				reason: BanReason::Misbehavior,
			},
		);

		return Err(anyhow!("Received malformed ping command from {node_endpoint}"));
	}

	let nonce = vec_to_u64_le(payload).context("failed to parse ping nonce")?;
	debug!("Received ping command from {} with nonce {}", node_endpoint, nonce);

	tcp_writer
		.send_message("pong", &nonce.to_le_bytes())
		.await
		.context("failed to send pong")
}

/// Handles an incoming pong message from a peer
fn handle_pong(node_endpoint: &NodeEndpoint) {
	// TODO: Handle pong properly
	debug!("Received pong from {}", node_endpoint);
}

/// Computes a deterministic score for relay peer selection
///
/// Uses hashing to produce a stable ranking of peers for a given address
/// within a 24-hour time bucket. Same inputs always produce same output
fn compute_relay_score(relay_key: u64, addr_hash: u64, time_bucket: u64, peer_hash: u64) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	relay_key.hash(&mut hasher);
	addr_hash.hash(&mut hasher);
	time_bucket.hash(&mut hasher);
	peer_hash.hash(&mut hasher);
	hasher.finish()
}

/// Relays addr entries to the top-scoring outgoing peers
///
/// For each recently-active entry, selects the best peers by deterministic
/// hash score and sends a single-entry addr message to each
async fn relay_addr(node_manager: &Arc<NodeManager>, source_endpoint: &NodeEndpoint, entries: &[&AddrEntry]) {
	if entries.is_empty() {
		return;
	}

	// unix_now() returns seconds; 86400 seconds per day
	#[allow(clippy::arithmetic_side_effects)]
	let time_bucket = unix_now() / (24 * 60 * 60);

	// Collect connected outgoing peers (excluding source), releasing DashMap lock
	let peers: Vec<(NodeEndpoint, SharedTcpWriter)> = node_manager
		.nodes
		.iter()
		.filter_map(|entry| {
			let node = entry.value();
			if node.endpoint == *source_endpoint {
				return None;
			}
			if node.connection_type != ConnectionType::Outgoing {
				return None;
			}
			if let NodeState::Connected { ref writer } = node.state {
				Some((entry.key().clone(), Arc::clone(writer)))
			} else {
				None
			}
		})
		.collect();

	if peers.is_empty() {
		return;
	}

	for entry in entries {
		// Hash the (address, port) tuple for a stable addr identifier
		let addr_hash = {
			use std::hash::{Hash, Hasher};
			let mut hasher = std::collections::hash_map::DefaultHasher::new();
			entry.address.address.hash(&mut hasher);
			entry.address.port.hash(&mut hasher);
			hasher.finish()
		};

		// Score each peer and sort descending
		let mut scored: Vec<(usize, u64)> = peers
			.iter()
			.enumerate()
			.map(|(idx, (ep, _))| {
				let peer_hash = {
					use std::hash::{Hash, Hasher};
					let mut hasher = std::collections::hash_map::DefaultHasher::new();
					ep.address.hash(&mut hasher);
					ep.port.hash(&mut hasher);
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
			let (ref ep, ref writer) = peers[idx];

			// Check and update addr_known under DashMap lock
			// Clear the set when the 24h time bucket rotates to prevent unbounded growth
			let already_known = if let Some(mut node) = node_manager.nodes.get_mut(ep) {
				if node.addr_known_bucket != time_bucket {
					node.addr_known.clear();
					node.addr_known_bucket = time_bucket;
				}
				!node.addr_known.insert((entry.address.address, entry.address.port))
			} else {
				continue;
			};

			if already_known {
				continue;
			}

			// Build a single-entry addr message preserving the original timestamp
			let relay_entry = AddrEntry {
				timestamp: entry.timestamp,
				address: NetworkAddress {
					services: entry.address.services,
					address: entry.address.address,
					port: entry.address.port,
				},
			};
			let msg = MessageAddr::from_entries(vec![relay_entry]);

			if let Err(e) = writer.send_message("addr", &msg.to_bytes()).await {
				debug!("Failed to relay addr to {}: {}", ep, e);
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
async fn handle_addr(node_manager: &Arc<NodeManager>, node_endpoint: &NodeEndpoint, payload: &[u8]) -> Result<()> {
	let msg = MessageAddr::from_bytes(payload).context("failed to parse addr message")?;

	debug!(
		"Received addr message with {} addresses from {}",
		msg.entries().len(),
		node_endpoint
	);

	// Rate limiting: refill tokens and consume one per entry
	let accepted_indices: Vec<usize> = {
		let Some(mut node) = node_manager.nodes.get_mut(node_endpoint) else {
			return Ok(());
		};

		let now_instant = Instant::now();
		let elapsed = now_instant.duration_since(node.last_token_refill);
		node.addr_tokens = refill_addr_tokens(node.addr_tokens, elapsed);
		node.last_token_refill = now_instant;

		let mut indices = Vec::new();
		for (i, entry) in msg.entries().iter().enumerate() {
			if node.addr_tokens >= 1.0 {
				// Finite f64 subtraction; both operands are bounded by ADDR_TOKEN_CAPACITY
				#[allow(clippy::arithmetic_side_effects, clippy::float_arithmetic)]
				{
					node.addr_tokens -= 1.0;
				}
				indices.push(i);
			} else {
				debug!(
					"Rate limited addr entry {} from {} (tokens exhausted)",
					entry.address.address, node_endpoint
				);
			}
		}

		drop(node);
		indices
	};

	if accepted_indices.is_empty() {
		return Ok(());
	}

	for &i in &accepted_indices {
		// msg.entries() is bounded by 1000 (validated in from_bytes), i < entries.len()
		#[allow(clippy::indexing_slicing)]
		let entry = &msg.entries()[i];

		if !is_recently_active(entry.timestamp) {
			debug!(
				"Addr: {} filtered out (timestamp={}, not recently active)",
				entry.address.address, entry.timestamp
			);
			continue;
		}

		let endpoint = NodeEndpoint {
			address: entry.address.address,
			port: entry.address.port,
		};

		// Try to revive Dead nodes with a newer timestamp
		if node_manager.revive_if_newer(&endpoint, entry.timestamp) {
			info!("Revived dead node {} with newer addr timestamp", endpoint);
			continue;
		}

		// Otherwise try to insert as new outgoing node
		debug!("Addr: {} is recently active, adding", entry.address.address);
		node_manager.insert_outgoing(entry.address.address, entry.address.port);
	}

	// Relay eligible entries to other peers
	// Only relay small batches from organic gossip (not getaddr responses)
	let should_relay = node_manager.nodes.get(node_endpoint).is_some_and(|n| !n.sent_getaddr);

	if should_relay && msg.entries().len() <= ADDR_RELAY_MAX_ENTRIES {
		let relay_entries: Vec<&AddrEntry> = accepted_indices
			.iter()
			.filter_map(|&i| {
				// Indices are bounded by msg.entries().len() from the rate limiting loop
				#[allow(clippy::indexing_slicing)]
				let entry = &msg.entries()[i];
				if is_recently_active(entry.timestamp) {
					Some(entry)
				} else {
					None
				}
			})
			.collect();

		relay_addr(node_manager, node_endpoint, &relay_entries).await;
	}

	Ok(())
}

/// Handles an incoming getaddr request from a peer
async fn handle_getaddr(
	node_manager: &Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
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
				address: node.endpoint.address,
				port: node.endpoint.port,
			}
		})
		.collect();

	if filtered_nodes.is_empty() {
		debug!(
			"No recently active outgoing nodes to share for GetAddr from {}",
			node_endpoint
		);
		return Ok(());
	}

	debug!("Sending list of {} nodes to {}", filtered_nodes.len(), node_endpoint);

	let message_addr = MessageAddr::new(filtered_nodes);
	tcp_writer
		.send_message("addr", &message_addr.to_bytes())
		.await
		.context("failed to send addr")
}

#[cfg(test)]
// Tests use unwrap for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;

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
		let state = NodeState::Banned {
			reason: BanReason::Misbehavior,
		};
		assert!(state.is_banned());
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
		let nm = NodeManager::new();
		let ip: IpAddr = "8.8.8.8".parse().unwrap();
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		assert_eq!(nm.get_external_ip(), Some(ip));
	}

	#[test]
	fn get_external_ip_requires_three_votes() {
		let nm = NodeManager::new();
		let ip: IpAddr = "8.8.8.8".parse().unwrap();
		nm.record_external_ip_vote(ip);
		nm.record_external_ip_vote(ip);
		assert_eq!(nm.get_external_ip(), None);
	}

	#[test]
	fn get_external_ip_returns_majority() {
		let nm = NodeManager::new();
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
		let nm = NodeManager::new();
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
		let nm = NodeManager::new();
		let ip: IpAddr = "1.2.3.4".parse().unwrap();
		nm.insert(ip, 12345, ConnectionType::Incoming);

		// Node starts in Handshaking, not Connected -- should return false
		assert!(!nm.has_incoming_connected());
	}

	#[test]
	fn addr_known_dedup_prevents_duplicate_insert() {
		let nm = NodeManager::new();
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		// First insert returns true (new)
		let first = nm
			.nodes
			.get_mut(&NodeEndpoint {
				address: ip,
				port: 9933,
			})
			.unwrap()
			.addr_known
			.insert((ip, 9933));
		assert!(first);

		// Second insert returns false (already known)
		let second = nm
			.nodes
			.get_mut(&NodeEndpoint {
				address: ip,
				port: 9933,
			})
			.unwrap()
			.addr_known
			.insert((ip, 9933));
		assert!(!second);
	}

	#[test]
	fn addr_known_clears_on_bucket_rotation() {
		let nm = NodeManager::new();
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let ep = NodeEndpoint {
			address: ip,
			port: 9933,
		};

		// Insert an addr and set a bucket
		{
			let mut node = nm.nodes.get_mut(&ep).unwrap();
			node.addr_known.insert(("1.2.3.4".parse::<IpAddr>().unwrap(), 9933));
			node.addr_known_bucket = 100;
		}

		// Simulate bucket rotation by checking a different bucket
		{
			let mut node = nm.nodes.get_mut(&ep).unwrap();
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
		let nm = NodeManager::new();
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let ep = NodeEndpoint {
			address: ip,
			port: 9933,
		};
		let node = nm.nodes.get(&ep).unwrap();
		assert!(!node.sent_getaddr);
	}

	#[test]
	fn sent_getaddr_can_be_set() {
		let nm = NodeManager::new();
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let ep = NodeEndpoint {
			address: ip,
			port: 9933,
		};
		{
			let mut node = nm.nodes.get_mut(&ep).unwrap();
			node.sent_getaddr = true;
		}
		let node = nm.nodes.get(&ep).unwrap();
		assert!(node.sent_getaddr);
	}

	#[test]
	fn new_node_starts_with_full_token_bucket() {
		let nm = NodeManager::new();
		let ip: IpAddr = "10.0.0.1".parse().unwrap();
		nm.insert(ip, 9933, ConnectionType::Outgoing);

		let ep = NodeEndpoint {
			address: ip,
			port: 9933,
		};
		let node = nm.nodes.get(&ep).unwrap();
		assert!((node.addr_tokens - ADDR_TOKEN_CAPACITY).abs() < f64::EPSILON);
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
}
