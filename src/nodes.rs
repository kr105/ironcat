// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use dashmap::DashMap;
use rand::RngCore;
use std::{
	ffi::CStr,
	fmt,
	io::{Cursor, Read},
	net::IpAddr,
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
	io::{self, AsyncWriteExt},
	net::TcpStream,
	sync::Mutex,
	time::{sleep, timeout},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
	network::{
		decode_varint, message_addr::MessageAddr, message_version::MessageVersion, Message, NetworkAddress,
		NetworkCommand, NetworkQueue, ServiceMask, SharedTcpWriter, SharedTcpWriterExt,
	},
	utils::{is_recently_active, u64_to_vec_le, vec_to_u64_le},
};

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

#[derive(Debug)]
pub struct NodeStats {
	pub total: usize,
	pub connected: usize,
	pub disconnected: usize,
}

#[derive(Debug, PartialEq, Eq)]
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

/// Represents a node in the Catcoin network
#[allow(clippy::struct_excessive_bools)]
pub struct Node {
	pub endpoint: NodeEndpoint,

	/// Node has sent the verack message
	pub ver_ack: bool,

	/// Unix timestamp
	pub last_seen: u64,

	pub timed_out: bool,

	pub connected: bool,

	pub version: u32,

	pub services: ServiceMask,

	pub timestamp: i64,

	pub user_agent: String,

	pub height: i32,

	pub relay: bool,

	/// Set if the node does something that shouldn't
	pub not_good: bool,

	pub tcp_writer: Option<SharedTcpWriter>,

	pub connection_type: ConnectionType,
}

/// Manages a collection of nodes in the network
pub struct NodeManager {
	// Using DashMap for concurrent access without needing explicit locking
	pub nodes: DashMap<NodeEndpoint, Node>,

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
				let node = Node {
					endpoint,
					ver_ack: false,
					last_seen: 0,
					timed_out: false,
					connected: false,
					height: 0,
					relay: false,
					services: ServiceMask::empty(),
					timestamp: 0,
					user_agent: String::new(),
					version: 0,
					not_good: false,
					tcp_writer: None,
					connection_type,
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

	/// Set the connected flag for the node
	pub fn set_connected(&self, node_endpoint: &NodeEndpoint, connected: bool) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.connected = connected;
		}
	}

	/// Updates the `timed_out` flag for a node
	pub fn set_timed_out(&self, node_endpoint: &NodeEndpoint, timed_out: bool) {
		if let Some(mut node) = self.nodes.get_mut(node_endpoint) {
			node.timed_out = timed_out;
		}
	}

	/// Checks if the node is appropiate to try a connection
	pub fn is_candidate(&self, node_endpoint: &NodeEndpoint) -> bool {
		if let Some(node) = self.nodes.get(node_endpoint) {
			return !(node.not_good || node.timed_out);
		}

		false
	}

	/// Sends a ping message to a connected node
	pub async fn send_ping(&self, node_endpoint: &NodeEndpoint) {
		let tcp_writer = if let Some(node) = self.nodes.get(node_endpoint) {
			if let Some(writer) = node.tcp_writer.clone() {
				writer
			} else {
				debug!("Skipping ping to {}, no active writer yet", node_endpoint);
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
		let connected = self.nodes.iter().filter(|n| n.connected).count();
		let disconnected = total.saturating_sub(connected);

		NodeStats {
			total,
			connected,
			disconnected,
		}
	}
}

/// Inserts a new outgoing connection node into the `NodeManager` and spawns a task to handle the connection
pub fn insert_node(node_manager: Arc<NodeManager>, address: IpAddr, port: u16) {
	if node_manager.insert(address, port, ConnectionType::Outgoing) {
		tokio::spawn(async move {
			handle_node_connection(node_manager, address, port).await;
		});
	}
}

/// Handles the connection to a node
async fn handle_node_connection(node_manager: Arc<NodeManager>, address: IpAddr, port: u16) {
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

	for attempt in 1..=max_attempts {
		match timeout(connect_timeout, TcpStream::connect((address, port))).await {
			Ok(Ok(stream)) => {
				info!("Connected to {} on attempt {}", &node_endpoint, attempt);
				tcp_stream = Some(stream);
				break;
			}
			Ok(Err(e)) => {
				if attempt < max_attempts {
					trace!("Failed to connect to {} on attempt {}: {}", &node_endpoint, attempt, e);
					sleep(delay_between_attempts).await;
				} else {
					debug!(
						"Failed to connect to {} after {} attempts: {}",
						&node_endpoint, max_attempts, e
					);
					node_manager.set_timed_out(&node_endpoint, true);
					return;
				}
			}
			Err(_) => {
				if attempt < max_attempts {
					trace!("Connection to {} timed out on attempt {}", &node_endpoint, attempt);
					sleep(delay_between_attempts).await;
				} else {
					debug!(
						"Connection to {} timed out after {} attempts",
						&node_endpoint, max_attempts
					);
					node_manager.set_timed_out(&node_endpoint, true);
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

	// Once the connection is established, the first step is to present ourselves
	// by sending a MessageVersion packet

	let network_address = NetworkAddress::new(address, port);
	let version = MessageVersion::new(network_address, node_manager.my_nonce);
	let packet = match Message::new("version", version.to_bytes()) {
		Ok(p) => p,
		Err(e) => {
			error!("Failed to create version message for {}: {}", &node_endpoint, e);
			node_manager.set_timed_out(&node_endpoint, true);
			return;
		}
	};

	if let Err(e) = tcp_stream.write_all(&packet.to_bytes()).await {
		error!("Failed to send version to {}: {}", &node_endpoint, e);
		node_manager.set_timed_out(&node_endpoint, true);
		return;
	}

	// Hand off the connection to the connection loop
	node_connection_loop(Arc::clone(&node_manager), node_endpoint.clone(), tcp_stream).await;

	// If the connection loop ended, it means that the connection was closed
	node_manager.set_connected(&node_endpoint, false);

	debug!("Exiting handle_node_connection for {:?}", node_endpoint);
}

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

/// Parses and handles an incoming message from a node
#[allow(clippy::too_many_lines)]
async fn parse_incoming_message(
	node_manager: Arc<NodeManager>,
	node_endpoint: &NodeEndpoint,
	tcp_writer: SharedTcpWriter,
	message: Message,
) -> Result<()> {
	// Extract the command from the message
	let command = match CStr::from_bytes_until_nul(&message.command) {
		Ok(str) => str
			.to_str()
			.map_err(|e| anyhow!("command field is not valid UTF-8: {e}"))?,
		Err(_) => return Err(anyhow!("Error parsing incoming message, 'command' field is malformed")),
	};

	let command = NetworkCommand::from_str(command).map_err(|()| anyhow!("failed to parse network command"))?;

	debug!("Received message: {:?} from {}", command, &node_endpoint);

	match command {
		NetworkCommand::Version => {
			let mut node = node_manager
				.nodes
				.get_mut(node_endpoint)
				.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager"))?;

			// Nodes can send only one version command
			if node.version > 0 {
				node.not_good = true;

				return Err(anyhow!("Node {} sent version command twice", &node_endpoint));
			}

			let version = MessageVersion::from_bytes(&message.payload).context("failed to parse version message")?;

			if version.nonce == node_manager.my_nonce {
				// Oops it is me!
				debug!("Closing connection to {} as it is self-connection", &node_endpoint);
				return Err(anyhow!("Node is myself"));
			}

			// We should send the version message too if the connection is inbound
			if node.connection_type == ConnectionType::Incoming {
				let network_address = NetworkAddress::new(node_endpoint.address, node_endpoint.port);
				let version_message = MessageVersion::new(network_address, node_manager.my_nonce);

				tcp_writer
					.send_message("version", version_message.to_bytes())
					.await
					.context("failed to send version reply for inbound connection")?;
			}

			// Save node data
			node.endpoint = node_endpoint.clone();
			node.services = version.services;
			node.timestamp = version.timestamp;
			node.user_agent = version.user_agent;
			node.height = version.start_height;
			node.version = version.version;
			node.relay = version.relay;

			drop(node);

			// We must answer with a verack
			tcp_writer
				.send_message("verack", Vec::new())
				.await
				.context("failed to send verack")?;
		}

		NetworkCommand::Verack => {
			let mut node = node_manager
				.nodes
				.get_mut(node_endpoint)
				.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager"))?;
			node.ver_ack = true;

			// If we already received the version command from the node
			// and the node already received the version from us (with this verack),
			// then the connection is ready
			if node.version > 0 {
				info!(
					"Connection ready with node {} version={}, blocks={}, user_agent={}",
					&node_endpoint, node.version, node.height, node.user_agent
				);

				node.connected = true;
				node.tcp_writer = Some(Arc::clone(&tcp_writer));

				drop(node);

				// Now that connection is established, ask for more nodes :)
				tcp_writer
					.send_message("getaddr", Vec::new())
					.await
					.context("failed to send getaddr")?;
			}
		}

		NetworkCommand::Ping => {
			// Only nonce should be present
			if message.payload.len() != 8 {
				warn!("Received malformed ping command from {}", &node_endpoint);

				let mut node = node_manager
					.nodes
					.get_mut(node_endpoint)
					.ok_or_else(|| anyhow!("node {node_endpoint} disappeared from manager"))?;
				node.not_good = true;
				drop(node);

				return Err(anyhow!("Received malformed ping command from {}", &node_endpoint));
			}

			let nonce = vec_to_u64_le(&message.payload).context("failed to parse ping nonce")?;
			debug!("Received ping command from {} with nonce {}", &node_endpoint, nonce);

			// Reply back with the nonce
			tcp_writer
				.send_message("pong", u64_to_vec_le(nonce))
				.await
				.context("failed to send pong")?;
		}

		NetworkCommand::Pong => {
			// TODO: Handle pong properly
			debug!("Received pong from {}", node_endpoint);
		}

		NetworkCommand::Addr => {
			let mut cursor = Cursor::new(message.payload.as_slice());
			let count = decode_varint(&mut cursor).context("failed to decode addr count")?;

			// Bitcoin/Catcoin protocol limits addr messages to 1000 entries
			if count > 1000 {
				return Err(anyhow!(
					"addr message from {node_endpoint} claims {count} entries, max is 1000"
				));
			}

			debug!("Received addr message with {} addresses from {}", count, &node_endpoint);

			// Check all the received addresses
			for _ in 0..count {
				let timestamp = cursor
					.read_u32::<LittleEndian>()
					.context("failed to read addr timestamp")?;

				let mut buffer = [0u8; 26];
				cursor
					.read_exact(&mut buffer)
					.context("failed to read addr network address bytes")?;

				let network_address =
					NetworkAddress::from_bytes(&buffer).context("failed to parse addr network address")?;

				if is_recently_active(timestamp) {
					debug!("Addr: {} is recently active, adding", network_address.address);
					insert_node(Arc::clone(&node_manager), network_address.address, network_address.port);
					debug!("{} nodes", node_manager.nodes.len());
				} else {
					debug!(
						"Addr: {} filtered out (timestamp={}, not recently active)",
						network_address.address, timestamp
					);
				}
			}
		}

		NetworkCommand::Alert => {
			debug!("Received an alert command from {}, ignoring it :)", &node_endpoint);
		}

		NetworkCommand::GetAddr => {
			// SystemTime::now().duration_since(UNIX_EPOCH) only fails if system clock
			// is before 1970, which is not a realistic scenario
			#[allow(clippy::expect_used)]
			let timestamp = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("Time went backwards")
				.as_secs();

			// Filter the node list so we share good nodes only
			let filtered_nodes: Vec<NetworkAddress> = node_manager
				.nodes
				.iter()
				.filter(|entry| {
					let node = entry.value();
					node.connected && (timestamp.saturating_sub(node.last_seen) < 60 * 60 * 2) && !node.not_good
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

			debug!("Sending list of {} nodes to {}", filtered_nodes.len(), &node_endpoint);

			let message_addr = MessageAddr::new(filtered_nodes);
			tcp_writer
				.send_message("addr", message_addr.to_bytes())
				.await
				.context("failed to send addr")?;
		}

		NetworkCommand::Unknown(str) => {
			error!("Received unknown network command from {}: {}", &node_endpoint, str);
		}
	}

	Ok(())
}
