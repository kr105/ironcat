// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use rand::RngCore;
use std::{
	ffi::CStr,
	fmt,
	net::IpAddr,
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
	io::{self, AsyncWriteExt},
	net::TcpStream,
	sync::Mutex,
	time::{sleep, timeout, Instant},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
	network::{
		message_addr::MessageAddr, message_version::MessageVersion, Message, NetworkAddress, NetworkCommand,
		NetworkQueue, ServiceMask, SharedTcpWriter, SharedTcpWriterExt,
	},
	utils::{is_recently_active, u64_to_vec_le, vec_to_u64_le},
};

/// Base delay for exponential backoff in seconds
const BACKOFF_BASE_SECS: u64 = 30;

/// Maximum backoff delay in seconds (30 minutes)
const BACKOFF_MAX_SECS: u64 = 30 * 60;

/// Maximum retry attempts before marking a node as dead
const MAX_RETRY_ATTEMPTS: u32 = 10;

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
	Connecting,
	/// TCP connected, version handshake in progress
	Handshaking,
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
	// Part of the NodeState public API, will be used when ban checking is implemented
	#[allow(dead_code)]
	pub const fn is_banned(&self) -> bool {
		matches!(self, Self::Banned { .. })
	}
}

impl fmt::Display for NodeState {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Connecting => write!(f, "Connecting"),
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
	// Will be used by TUI enhancements and filtering logic
	#[allow(dead_code)]
	pub connected: bool,
	pub connection_type: ConnectionType,
	// Will be used by TUI detail view
	#[allow(dead_code)]
	pub user_agent: String,
	// Will be used by TUI detail view
	#[allow(dead_code)]
	pub version: u32,
	pub state_label: String,
}

/// Represents a node in the Catcoin network
pub struct Node {
	pub endpoint: NodeEndpoint,

	/// Unix timestamp
	pub last_seen: u64,

	pub version: u32,

	pub services: ServiceMask,

	pub timestamp: i64,

	pub user_agent: String,

	pub height: i32,

	pub relay: bool,

	pub connection_type: ConnectionType,

	/// Current connection lifecycle state
	pub state: NodeState,
}

/// Manages a collection of nodes in the network
pub struct NodeManager {
	// Using DashMap for concurrent access without needing explicit locking
	pub(crate) nodes: DashMap<NodeEndpoint, Node>,

	// Used to avoid connecting to myself
	pub my_nonce: u64,
}

impl Default for NodeManager {
	fn default() -> Self {
		Self::new()
	}
}

impl NodeManager {
	pub fn new() -> Self {
		Self {
			nodes: DashMap::new(),
			my_nonce: rand::thread_rng().next_u64(),
		}
	}

	/// Inserts a new node into the manager if it doesn't already exist
	///
	/// Returns true if the node was inserted, false if it already existed
	pub fn insert(&self, address: IpAddr, port: u16, connection_type: ConnectionType) -> bool {
		let endpoint = NodeEndpoint { address, port };

		// Use entry API for atomic check-and-insert
		let entry = self.nodes.entry(endpoint.clone());

		match entry {
			dashmap::mapref::entry::Entry::Occupied(_) => false,
			dashmap::mapref::entry::Entry::Vacant(vacant) => {
				let initial_state = match connection_type {
					ConnectionType::Outgoing => NodeState::Connecting,
					ConnectionType::Incoming => NodeState::Handshaking,
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
					connection_type,
					state: initial_state,
				};

				vacant.insert(node);
				true
			}
		}
	}

	/// Updates the `last_seen` timestamp for a node with the current time
	pub fn update_last_seen(&self, node_endpoint: &NodeEndpoint) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			// SystemTime::now().duration_since(UNIX_EPOCH) only fails if system clock
			// is before 1970, which is not a realistic scenario
			#[allow(clippy::expect_used)]
			let now = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("Time went backwards")
				.as_secs();
			node.last_seen = now;
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
			return matches!(node.state, NodeState::Connecting) || node.state.is_connectable();
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
		let nonce_bytes = nonce.to_le_bytes();

		if let Err(e) = tcp_writer.send_message("ping", Vec::from(nonce_bytes)).await {
			warn!("Failed to send ping to {}: {}", node_endpoint, e);
		}
	}

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
					connected: node.state.is_connected(),
					connection_type: node.connection_type,
					user_agent: node.user_agent.clone(),
					version: node.version,
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
		let scan_interval = Duration::from_secs(30);

		loop {
			tokio::time::sleep(scan_interval).await;

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
					node.state = NodeState::Connecting;
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

	/// Marks a node as permanently banned
	// Part of the NodeManager encapsulation API, will replace direct state mutation
	#[allow(dead_code)]
	pub fn ban_node(&self, node_endpoint: &NodeEndpoint, reason: BanReason) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.state = NodeState::Banned { reason };
		}
	}

	/// Revives a Dead node if the given timestamp is newer than `last_seen`
	///
	/// Returns true if the node was revived, false otherwise
	pub fn revive_if_newer(self: &Arc<Self>, endpoint: &NodeEndpoint, timestamp: u32) -> bool {
		if let Some(mut node) = self.nodes.get_mut(endpoint) {
			if matches!(node.state, NodeState::Dead) && u64::from(timestamp) > node.last_seen {
				node.state = NodeState::Disconnected {
					retry_at: Instant::now(),
					attempt: 0,
				};
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
}

/// Handles the connection to a node
// Connection lifecycle (connect + handshake + backoff transitions) is inherently long
#[allow(clippy::too_many_lines)]
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
	let max_attempts = 3;
	let delay_between_attempts = Duration::from_secs(30);
	let connect_timeout = Duration::from_secs(10);

	for tcp_attempt in 1..=max_attempts {
		match timeout(connect_timeout, TcpStream::connect((address, port))).await {
			Ok(Ok(stream)) => {
				info!("Connected to {} on attempt {}", &node_endpoint, tcp_attempt);
				tcp_stream = Some(stream);
				break;
			}
			Ok(Err(e)) => {
				if tcp_attempt < max_attempts {
					trace!(
						"Failed to connect to {} on attempt {}: {}",
						&node_endpoint,
						tcp_attempt,
						e
					);
					sleep(delay_between_attempts).await;
				} else {
					debug!(
						"Failed to connect to {} after {} attempts: {}",
						&node_endpoint, max_attempts, e
					);
					let next_attempt = attempt.saturating_add(1);
					if next_attempt >= MAX_RETRY_ATTEMPTS {
						info!("Node {} exhausted all retry attempts, marking dead", &node_endpoint);
						node_manager.set_state(&node_endpoint, NodeState::Dead);
					} else {
						let backoff = calculate_backoff(next_attempt);
						debug!(
							"Node {} failed to connect, retry {} in {:?}",
							&node_endpoint, next_attempt, backoff
						);
						// Instant::now() + bounded Duration cannot overflow in practice
						#[allow(clippy::arithmetic_side_effects)]
						let retry_at = Instant::now() + backoff;
						node_manager.set_state(
							&node_endpoint,
							NodeState::Disconnected {
								retry_at,
								attempt: next_attempt,
							},
						);
					}
					return;
				}
			}
			Err(_) => {
				if tcp_attempt < max_attempts {
					trace!("Connection to {} timed out on attempt {}", &node_endpoint, tcp_attempt);
					sleep(delay_between_attempts).await;
				} else {
					debug!(
						"Connection to {} timed out after {} attempts",
						&node_endpoint, max_attempts
					);
					let next_attempt = attempt.saturating_add(1);
					if next_attempt >= MAX_RETRY_ATTEMPTS {
						info!("Node {} exhausted all retry attempts, marking dead", &node_endpoint);
						node_manager.set_state(&node_endpoint, NodeState::Dead);
					} else {
						let backoff = calculate_backoff(next_attempt);
						debug!(
							"Node {} timed out, retry {} in {:?}",
							&node_endpoint, next_attempt, backoff
						);
						// Instant::now() + bounded Duration cannot overflow in practice
						#[allow(clippy::arithmetic_side_effects)]
						let retry_at = Instant::now() + backoff;
						node_manager.set_state(
							&node_endpoint,
							NodeState::Disconnected {
								retry_at,
								attempt: next_attempt,
							},
						);
					}
					return;
				}
			}
		}
	}

	let Some(mut tcp_stream) = tcp_stream else {
		error!(
			"Failed to establish connection to {:?} after {} attempts",
			node_endpoint, max_attempts
		);
		return;
	};

	// TCP connected, transition to handshaking
	node_manager.set_state(&node_endpoint, NodeState::Handshaking);

	// Once the connection is established, the first step is to present ourselves
	// by sending a MessageVersion packet

	let network_address = NetworkAddress::new(address, port);
	let version = MessageVersion::new(network_address, node_manager.my_nonce);
	let packet = match Message::new("version", version.to_bytes()) {
		Ok(p) => p,
		Err(e) => {
			error!("Failed to create version message for {}: {}", &node_endpoint, e);
			let next_attempt = attempt.saturating_add(1);
			if next_attempt >= MAX_RETRY_ATTEMPTS {
				info!("Node {} exhausted all retry attempts, marking dead", &node_endpoint);
				node_manager.set_state(&node_endpoint, NodeState::Dead);
			} else {
				let backoff = calculate_backoff(next_attempt);
				debug!(
					"Node {} version message failed, retry {} in {:?}",
					&node_endpoint, next_attempt, backoff
				);
				// Instant::now() + bounded Duration cannot overflow in practice
				#[allow(clippy::arithmetic_side_effects)]
				let retry_at = Instant::now() + backoff;
				node_manager.set_state(
					&node_endpoint,
					NodeState::Disconnected {
						retry_at,
						attempt: next_attempt,
					},
				);
			}
			return;
		}
	};

	if let Err(e) = tcp_stream.write_all(&packet.to_bytes()).await {
		error!("Failed to send version to {}: {}", &node_endpoint, e);
		let next_attempt = attempt.saturating_add(1);
		if next_attempt >= MAX_RETRY_ATTEMPTS {
			info!("Node {} exhausted all retry attempts, marking dead", &node_endpoint);
			node_manager.set_state(&node_endpoint, NodeState::Dead);
		} else {
			let backoff = calculate_backoff(next_attempt);
			debug!(
				"Node {} version send failed, retry {} in {:?}",
				&node_endpoint, next_attempt, backoff
			);
			// Instant::now() + bounded Duration cannot overflow in practice
			#[allow(clippy::arithmetic_side_effects)]
			let retry_at = Instant::now() + backoff;
			node_manager.set_state(
				&node_endpoint,
				NodeState::Disconnected {
					retry_at,
					attempt: next_attempt,
				},
			);
		}
		return;
	}

	// Hand off the connection to the connection loop
	node_connection_loop(Arc::clone(&node_manager), node_endpoint.clone(), tcp_stream).await;

	// Connection loop ended -- transition based on retry count
	// attempt + 1 is bounded by MAX_RETRY_ATTEMPTS (10), no overflow
	#[allow(clippy::arithmetic_side_effects)]
	let next_attempt = attempt + 1;
	if next_attempt >= MAX_RETRY_ATTEMPTS {
		debug!("Node {} exhausted all retry attempts, marking as dead", &node_endpoint);
		node_manager.set_state(&node_endpoint, NodeState::Dead);
	} else {
		let backoff = calculate_backoff(next_attempt);
		debug!(
			"Node {} disconnected, will retry in {}s (attempt {})",
			&node_endpoint,
			backoff.as_secs(),
			next_attempt
		);
		// Instant::now() + bounded Duration cannot overflow in practice
		#[allow(clippy::arithmetic_side_effects)]
		let retry_at = Instant::now() + backoff;
		node_manager.set_state(
			&node_endpoint,
			NodeState::Disconnected {
				retry_at,
				attempt: next_attempt,
			},
		);
	}

	debug!("Exiting handle_node_connection for {:?}", node_endpoint);
}

/// Main read loop for a node connection
pub async fn node_connection_loop(node_manager: Arc<NodeManager>, node_endpoint: NodeEndpoint, tcp_stream: TcpStream) {
	// Split the TCP stream into separate reader and writer
	let (mut tcp_reader, tcp_writer) = tcp_stream.into_split();

	// Wrap the writer on Arc<Mutex> so we can write from multiple places later on
	let shared_writer: SharedTcpWriter = Arc::new(Mutex::new(tcp_writer));

	let ping_timeout = Duration::from_secs(180); // 3 minutes

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
			() = sleep(ping_timeout) => {
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

	let command = NetworkCommand::from_str(command).map_err(|()| {
		warn!("Failed to parse network command from {}", node_endpoint);
		anyhow!("failed to parse network command")
	})?;

	debug!("Received message: {:?} from {}", command, node_endpoint);

	match command {
		NetworkCommand::Version => handle_version(&node_manager, node_endpoint, &tcp_writer, &message.payload).await,
		NetworkCommand::Verack => handle_verack(&node_manager, node_endpoint, &tcp_writer).await,
		NetworkCommand::Ping => handle_ping(&node_manager, node_endpoint, &tcp_writer, &message.payload).await,
		NetworkCommand::Pong => {
			handle_pong(node_endpoint);
			Ok(())
		}
		NetworkCommand::Addr => handle_addr(&node_manager, node_endpoint, &message.payload),
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
		if node.version > 0 {
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
			};
			warn!("Node {} sent version command twice, banning", node_endpoint);
			return Err(anyhow!("Node {node_endpoint} sent version command twice"));
		}

		if version.nonce == node_manager.my_nonce {
			debug!("Closing connection to {} as it is self-connection", node_endpoint);
			return Err(anyhow!("Node is myself"));
		}

		node.connection_type == ConnectionType::Incoming
		// RefMut dropped here
	};

	// Phase 2: Async sends without holding any lock
	if is_incoming {
		let network_address = NetworkAddress::new(node_endpoint.address, node_endpoint.port);
		let version_message = MessageVersion::new(network_address, node_manager.my_nonce);
		tcp_writer
			.send_message("version", version_message.to_bytes())
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
		node.relay = version.relay;
	}

	// Verack sent after all locks released
	tcp_writer
		.send_message("verack", Vec::new())
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
	if node.version > 0 {
		info!(
			"Connection ready with node {} version={}, blocks={}, user_agent={}",
			node_endpoint, node.version, node.height, node.user_agent
		);

		node.state = NodeState::Connected {
			writer: Arc::clone(tcp_writer),
		};

		drop(node);

		tcp_writer
			.send_message("getaddr", Vec::new())
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
		.send_message("pong", u64_to_vec_le(nonce))
		.await
		.context("failed to send pong")
}

/// Handles an incoming pong message from a peer
fn handle_pong(node_endpoint: &NodeEndpoint) {
	// TODO: Handle pong properly
	debug!("Received pong from {}", node_endpoint);
}

/// Handles an incoming addr message containing peer addresses
fn handle_addr(node_manager: &Arc<NodeManager>, node_endpoint: &NodeEndpoint, payload: &[u8]) -> Result<()> {
	let msg = MessageAddr::from_bytes(payload).context("failed to parse addr message")?;

	debug!(
		"Received addr message with {} addresses from {}",
		msg.entries().len(),
		node_endpoint
	);

	for entry in msg.entries() {
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

	Ok(())
}

/// Handles an incoming getaddr request from a peer
async fn handle_getaddr(
	node_manager: &Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
	// SystemTime::now().duration_since(UNIX_EPOCH) only fails if system clock
	// is before 1970, which is not a realistic scenario
	#[allow(clippy::expect_used)]
	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs();

	// Filter the node list so we share only recently active outgoing connections
	// Incoming connections use ephemeral OS ports -- their stored port is not their real listening port
	let filtered_nodes: Vec<NetworkAddress> = node_manager
		.nodes
		.iter()
		.filter(|entry| {
			let node = entry.value();
			node.state.is_connected()
				&& node.connection_type == ConnectionType::Outgoing
				&& (timestamp.saturating_sub(node.last_seen) < 60 * 60 * 2)
		})
		.take(1000) // Protocol limits to maximum of 1000 entries per addr message
		.map(|entry| {
			let node = entry.value();
			NetworkAddress {
				services: node.services.clone(),
				address: node.endpoint.address,
				port: node.endpoint.port,
			}
		})
		.collect();

	if filtered_nodes.is_empty() {
		error!("Can't answer the GetAddr message if the generated node list is empty");
		return Ok(());
	}

	debug!("Sending list of {} nodes to {}", filtered_nodes.len(), node_endpoint);

	let message_addr = MessageAddr::new(filtered_nodes);
	tcp_writer
		.send_message("addr", message_addr.to_bytes())
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
		assert!(!NodeState::Connecting.is_connected());
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
		assert!(!NodeState::Connecting.is_banned());
	}

	#[test]
	fn ban_reason_display() {
		assert_eq!(format!("{}", BanReason::ProtocolViolation), "protocol violation");
		assert_eq!(format!("{}", BanReason::Misbehavior), "misbehavior");
	}
}
