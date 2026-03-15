// SPDX-License-Identifier: Apache-2.0

pub mod block_download;
pub mod handler_addr;
mod handler_block;
mod handler_getblocks;
mod handler_headers;
pub mod handler_inv;
mod handler_mempool;
mod handler_ping;
mod handler_tx;
mod handler_verack;
pub mod handler_version;
pub mod test_helpers;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use rand::Rng;
use std::{
	collections::HashSet,
	ffi::CStr,
	fmt,
	net::IpAddr,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
	},
	time::Duration,
};
use tokio::{
	io::{self, AsyncWriteExt},
	net::TcpStream,
	sync::Mutex,
	time::{Instant, sleep, timeout},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
	difficulty::ConsensusParams,
	dns::DEFAULT_PORT,
	headers::HeaderStore,
	network::{
		Message, NetworkAddress, NetworkCommand, NetworkQueue, ServiceMask, SharedTcpWriter, SharedTcpWriterExt,
		message_addr::MessageAddr, message_version::MessageVersion,
	},
	storage::header_store_backend::HeaderStoreBackend,
	types::{block::BlockHeader, hash::Hash256},
	utils::{is_routable, unix_now},
};

/// Base delay for exponential backoff in seconds
pub const BACKOFF_BASE_SECS: u64 = 30;

/// Maximum backoff delay in seconds (30 minutes)
pub const BACKOFF_MAX_SECS: u64 = 30 * 60;

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
pub const REAPER_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Window for "recently active" outgoing nodes in `GetAddr` responses (2 hours)
const GETADDR_RECENT_WINDOW: u64 = 60 * 60 * 2;

/// Maximum entries in an addr message eligible for relay
const ADDR_RELAY_MAX_ENTRIES: usize = 10;

/// Number of peers to relay each addr entry to
const ADDR_RELAY_PEER_COUNT: usize = 2;

/// Token bucket refill rate: 1 token per 10 seconds
const ADDR_TOKEN_RATE: f64 = 0.1;

/// Token bucket maximum capacity
pub const ADDR_TOKEN_CAPACITY: f64 = 1000.0;

/// Token bucket initial fill for new/reactivated connections
pub const ADDR_TOKEN_INITIAL: f64 = 10.0;

/// How often to announce our own address to peers
const SELF_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Maximum length of user agent string after sanitization
pub const MAX_USER_AGENT_DISPLAY: usize = 256;

/// Maximum number of entries in the external IP votes map
const MAX_EXTERNAL_IP_VOTES: usize = 100;

/// Timeout for the self-connectivity probe
const REACHABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum entries in a peer's `addr_known` set within a 24h bucket
const MAX_ADDR_KNOWN: usize = 5000;

/// Maximum entries in a peer's `inv_known` set before clearing
const MAX_INV_KNOWN: usize = 50_000;

/// Maximum timeout strikes before a peer is excluded from download requests
pub const MAX_TIMEOUT_STRIKES: u8 = 5;

/// Time between strike decay decrements (5 minutes)
const STRIKE_DECAY_INTERVAL: Duration = Duration::from_secs(300);

/// How long before a header request is considered timed out
const HEADER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum protocol version that supports sendheaders (BIP 130)
const SENDHEADERS_VERSION: u32 = 70012;

/// Maximum concurrent incoming connections
pub const MAX_INCOMING_CONNECTIONS: usize = 125;

/// Cooldown period before the same IP can make a new incoming connection
pub const INCOMING_IP_COOLDOWN: Duration = Duration::from_secs(30);

/// How long a ban lasts in seconds (24 hours)
pub const BAN_DURATION_SECS: u64 = 24 * 60 * 60;

/// How long a connection can sit idle before being dropped (2x `PING_INTERVAL`)
const IDLE_TIMEOUT: Duration = Duration::from_secs(360);

/// Sender half of the block forwarding channel
type BlockSender = tokio::sync::mpsc::Sender<(Hash256, Vec<u8>)>;

/// Sender half of the peer disconnect notification channel
type DisconnectSender = tokio::sync::mpsc::UnboundedSender<IpAddr>;

/// Sender half of the reorg notification channel (carries new tip height)
type ReorgSender = tokio::sync::mpsc::UnboundedSender<u32>;

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
			rand::rng().next_u64() % jitter_window
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
	/// Connected incoming peers
	pub incoming: usize,
	/// Connected outgoing peers
	pub outgoing: usize,
	/// Nodes in Handshaking state
	pub handshaking: usize,
	/// Nodes in Connecting state
	pub connecting: usize,
	/// Nodes in Disconnected state
	pub disconnected: usize,
	/// Nodes in Dead state
	pub dead: usize,
	/// Nodes in Banned state
	pub banned: usize,
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

	/// Returns a short 2-char label for compact table display
	pub const fn short(&self) -> &'static str {
		match self {
			Self::Connecting => "Cn",
			Self::Handshaking => "Hs",
			Self::Connected => "Co",
			Self::Disconnected(_) => "Dc",
			Self::Dead => "De",
			Self::Banned => "Ba",
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
	/// Protocol version reported by peer
	pub version: u32,
	/// User agent string from version message
	pub user_agent: String,
	/// Unix timestamp of last message received from this peer
	pub last_seen: u64,
	/// Number of timeout strikes for display
	pub timeout_strikes: u8,
	/// Whether peer is excluded from download requests
	pub strike_excluded: bool,
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

	/// Number of accumulated timeout strikes
	pub timeout_strikes: u8,

	/// When the last strike was recorded (for decay timing)
	pub last_strike: Option<Instant>,

	/// Whether this peer is excluded from block/header requests
	pub strike_excluded: bool,

	/// When the current pending getheaders request was sent
	pub pending_getheaders: Option<Instant>,
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
			timeout_strikes: 0,
			last_strike: None,
			strike_excluded: false,
			pending_getheaders: None,
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
	pub nodes: DashMap<IpAddr, Node>,

	/// Random nonce for self-connection detection
	pub my_nonce: u64,

	/// Per-session random key for deterministic addr relay peer selection
	pub(crate) relay_key: u64,

	/// Peer votes for our external IP address
	pub external_ip_votes: DashMap<IpAddr, u32>,

	/// The port we are listening on (from CLI or default)
	pub listen_port: u16,

	/// Whether our listen port has been confirmed reachable from the outside.
	/// Set by probing our own `external_ip:listen_port` via TCP connect
	port_reachable: AtomicBool,

	/// Number of currently active incoming connections
	pub incoming_count: Arc<AtomicUsize>,

	/// Per-IP cooldown for incoming connections to prevent reconnect spam
	incoming_cooldowns: DashMap<IpAddr, Instant>,

	/// In-memory block header chain
	pub header_store: Arc<parking_lot::RwLock<HeaderStore>>,

	/// Cached chain tip height for lock-free TUI reads
	cached_height: AtomicU32,

	/// Cached chain tip nBits for lock-free TUI reads
	cached_tip_bits: AtomicU32,

	/// Channel sender for forwarding raw block payloads to the download manager
	block_sender: parking_lot::Mutex<Option<BlockSender>>,

	/// Channel sender for notifying the download manager when a peer disconnects
	disconnect_sender: parking_lot::Mutex<Option<DisconnectSender>>,

	/// Channel sender for notifying the download manager after a reorg
	reorg_sender: parking_lot::Mutex<Option<ReorgSender>>,

	/// Cached best contiguous block height for lock-free TUI reads
	cached_block_height: AtomicU32,

	/// Shared mempool for transaction relay
	mempool: parking_lot::Mutex<Option<Arc<tokio::sync::RwLock<crate::mempool::Mempool>>>>,

	/// Shared chainstate for UTXO lookups (needed by tx handler)
	chainstate: parking_lot::Mutex<Option<Arc<crate::chainstate::ChainState>>>,

	/// Shared block store for serving blocks to peers
	block_store: parking_lot::Mutex<Option<Arc<crate::storage::block_store::BlockStore>>>,

	/// Timestamp of the last connected block (for TUI display)
	last_block_time: parking_lot::Mutex<Option<Instant>>,

	/// Transaction count in the last connected block (for TUI display)
	last_block_tx_count: AtomicU32,
}

impl NodeManager {
	/// Creates a new `NodeManager` with an empty node set and random nonce and relay key
	///
	/// Headers are kept in-memory only (no persistence backend)
	pub fn new(genesis_header: BlockHeader, params: ConsensusParams) -> Self {
		Self::with_header_backend(genesis_header, params, None)
	}

	/// Creates a new `NodeManager` with an optional header persistence backend
	///
	/// If a backend is provided, headers are loaded from disk on startup and
	/// written through on every insert. If `None`, behaves identically to `new()`
	pub fn with_header_backend(
		genesis_header: BlockHeader,
		params: ConsensusParams,
		backend: Option<Arc<dyn HeaderStoreBackend>>,
	) -> Self {
		let mut rng = rand::rng();
		let store = HeaderStore::with_backend(genesis_header, params, backend);
		let initial_height = store.height();
		let initial_bits = store.tip_bits();
		Self {
			nodes: DashMap::new(),
			my_nonce: rng.next_u64(),
			relay_key: rng.next_u64(),
			external_ip_votes: DashMap::new(),
			listen_port: DEFAULT_PORT,
			port_reachable: AtomicBool::new(false),
			incoming_count: Arc::new(AtomicUsize::new(0)),
			incoming_cooldowns: DashMap::new(),
			header_store: Arc::new(parking_lot::RwLock::new(store)),
			cached_height: AtomicU32::new(initial_height),
			cached_tip_bits: AtomicU32::new(initial_bits),
			block_sender: parking_lot::Mutex::new(None),
			disconnect_sender: parking_lot::Mutex::new(None),
			reorg_sender: parking_lot::Mutex::new(None),
			cached_block_height: AtomicU32::new(0),
			mempool: parking_lot::Mutex::new(None),
			chainstate: parking_lot::Mutex::new(None),
			block_store: parking_lot::Mutex::new(None),
			last_block_time: parking_lot::Mutex::new(None),
			last_block_tx_count: AtomicU32::new(0),
		}
	}

	/// Attaches the block channel for forwarding received blocks to the download manager
	pub fn set_block_sender(&self, sender: BlockSender) {
		*self.block_sender.lock() = Some(sender);
	}

	/// Attaches the disconnect notification channel for the download manager
	pub fn set_disconnect_sender(&self, sender: DisconnectSender) {
		*self.disconnect_sender.lock() = Some(sender);
	}

	/// Attaches the reorg notification channel for the download manager
	pub fn set_reorg_sender(&self, sender: ReorgSender) {
		*self.reorg_sender.lock() = Some(sender);
	}

	/// Attaches the shared mempool for transaction relay and serving
	pub fn set_mempool(&self, mempool: Arc<tokio::sync::RwLock<crate::mempool::Mempool>>) {
		*self.mempool.lock() = Some(mempool);
	}

	/// Returns the shared mempool, or `None` if not yet attached
	pub fn mempool(&self) -> Option<Arc<tokio::sync::RwLock<crate::mempool::Mempool>>> {
		self.mempool.lock().as_ref().map(Arc::clone)
	}

	/// Attaches the shared chainstate for UTXO lookups
	pub fn set_chainstate(&self, chainstate: Arc<crate::chainstate::ChainState>) {
		*self.chainstate.lock() = Some(chainstate);
	}

	/// Returns the shared chainstate, or `None` if not yet attached
	pub fn chainstate(&self) -> Option<Arc<crate::chainstate::ChainState>> {
		self.chainstate.lock().as_ref().map(Arc::clone)
	}

	/// Attaches the shared block store for serving blocks to peers
	pub fn set_block_store(&self, store: Arc<crate::storage::block_store::BlockStore>) {
		*self.block_store.lock() = Some(store);
	}

	/// Returns the shared block store, or `None` if not yet attached
	pub fn block_store(&self) -> Option<Arc<crate::storage::block_store::BlockStore>> {
		self.block_store.lock().as_ref().map(Arc::clone)
	}

	/// Notifies the download manager that a peer has disconnected so
	/// its in-flight block requests can be immediately reassigned
	fn notify_peer_disconnected(&self, peer: &IpAddr) {
		if let Some(sender) = self.disconnect_sender.lock().as_ref() {
			let _ = sender.send(*peer);
		}
	}

	/// Returns the best contiguous block height for TUI display
	pub fn block_height(&self) -> u32 {
		self.cached_block_height.load(Ordering::Relaxed)
	}

	/// Updates the cached block height from the download manager
	pub fn set_block_height(&self, height: u32) {
		self.cached_block_height.store(height, Ordering::Relaxed);
	}

	/// Records that a block was just connected, for TUI display
	pub fn record_block_connected(&self, tx_count: u32) {
		*self.last_block_time.lock() = Some(Instant::now());
		self.last_block_tx_count.store(tx_count, Ordering::Relaxed);
	}

	/// Returns the time elapsed since the last block was connected
	pub fn last_block_elapsed(&self) -> Option<std::time::Duration> {
		self.last_block_time.lock().map(|t| t.elapsed())
	}

	/// Returns the transaction count of the last connected block
	pub fn last_block_tx_count(&self) -> u32 {
		self.last_block_tx_count.load(Ordering::Relaxed)
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
				limit = MAX_INCOMING_CONNECTIONS,
				peer = %address,
				port,
				"incoming connection limit reached, rejecting"
			);
			return None;
		}

		// Reject connections from our own external IP or loopback (e.g. reachability
		// probes). These would create ghost entries in the node map and waste
		// incoming slots
		if address.is_loopback() || self.get_external_ip().is_some_and(|ip| ip == address) {
			debug!(peer = %address, "rejecting incoming from own address");
			return None;
		}

		// Per-IP cooldown: reject if this IP connected too recently
		if let Some(last) = self.incoming_cooldowns.get(&address)
			&& last.elapsed() < INCOMING_IP_COOLDOWN
		{
			debug!(peer = %address, port, "rejecting (per-IP cooldown)");
			return None;
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
			debug!(limit = MAX_NODES, peer = %address, port, "node limit reached, not adding");
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
						node.pending_getheaders = None;
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
				node.pending_getheaders = None;
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
		let nonce = rand::rng().next_u64();

		let tcp_writer = if let Some(mut node) = self.nodes.get_mut(address) {
			let writer = if let NodeState::Connected { ref writer } = node.state {
				Arc::clone(writer)
			} else {
				debug!(peer = %address, "skipping ping, not connected");
				return;
			};
			node.last_ping_nonce = Some(nonce);
			writer
		} else {
			debug!(peer = %address, "skipping ping, node not found");
			return;
		};

		debug!(peer = %address, "sending ping");

		if let Err(e) = tcp_writer.send_message("ping", &nonce.to_le_bytes()).await {
			warn!(peer = %address, error = %e, "failed to send ping");
		}
	}

	/// Attaches a header persistence backend, loading stored headers from disk
	///
	/// The heavy loading (read + hash validation) runs before acquiring the
	/// write lock. Only the final pointer swap holds the lock, so the TUI
	/// and other readers are not blocked during the load
	pub fn attach_header_backend(&self, backend: Arc<dyn HeaderStoreBackend>) {
		let genesis_hash = self.header_store.read().genesis();

		// Slow: read all headers from redb and validate chain continuity
		let result = HeaderStore::try_load_from_backend(backend.as_ref(), genesis_hash);

		// Fast: swap pre-built maps into the store under write lock
		self.header_store.write().apply_backend_load(result, backend);
		self.refresh_chain_cache();
	}

	/// Returns the number of nodes currently in Connected state
	pub fn connected_count(&self) -> usize {
		self.nodes.iter().filter(|e| e.value().state.is_connected()).count()
	}

	/// Increments a peer's timeout strike counter and returns the new value
	///
	/// Sets `strike_excluded` when the counter reaches `MAX_TIMEOUT_STRIKES`.
	/// No-op for peers already at MAX (avoids resetting the decay clock).
	/// Returns 0 if the peer is not found
	pub fn increment_strike(&self, peer: &IpAddr) -> u8 {
		let Some(mut node) = self.nodes.get_mut(peer) else {
			return 0;
		};
		if node.timeout_strikes >= MAX_TIMEOUT_STRIKES {
			return node.timeout_strikes;
		}
		node.timeout_strikes = node.timeout_strikes.saturating_add(1);
		node.last_strike = Some(Instant::now());
		if node.timeout_strikes >= MAX_TIMEOUT_STRIKES {
			node.strike_excluded = true;
		}
		node.timeout_strikes
	}

	/// Performs one pass of strike decay across all peers
	///
	/// For each peer with strikes and an expired decay interval, decrements
	/// by 1. When strikes reach 0, clears exclusion and `last_strike`
	pub fn decay_strikes(&self) {
		for mut entry in self.nodes.iter_mut() {
			let node = entry.value_mut();
			if node.timeout_strikes == 0 {
				continue;
			}
			let Some(last) = node.last_strike else {
				continue;
			};
			if last.elapsed() < STRIKE_DECAY_INTERVAL {
				continue;
			}
			node.timeout_strikes = node.timeout_strikes.saturating_sub(1);
			if node.timeout_strikes == 0 {
				node.strike_excluded = false;
				node.last_strike = None;
			} else {
				node.last_strike = Some(Instant::now());
			}
		}
	}

	/// Background loop that checks for strike decay every 60 seconds
	///
	/// Each peer decays by 1 strike per `STRIKE_DECAY_INTERVAL` (5 minutes).
	/// A fully excluded peer (5 strikes) takes at least 25 minutes to recover
	pub async fn run_strike_decay(self: Arc<Self>) {
		loop {
			tokio::time::sleep(Duration::from_secs(60)).await;
			self.decay_strikes();
		}
	}

	/// Performs one pass of the header timeout scanner
	///
	/// Checks all peers for expired `pending_getheaders` requests and
	/// increments strikes for slow responders
	pub fn scan_header_timeouts(&self) {
		let mut timed_out_peers: Vec<IpAddr> = Vec::new();

		for mut entry in self.nodes.iter_mut() {
			let node = entry.value_mut();
			// Skip peers already excluded -- no point striking them further
			if node.strike_excluded {
				node.pending_getheaders = None;
				continue;
			}
			if let Some(sent_at) = node.pending_getheaders
				&& sent_at.elapsed() > HEADER_REQUEST_TIMEOUT
			{
				node.pending_getheaders = None;
				timed_out_peers.push(*entry.key());
			}
		}

		for peer in &timed_out_peers {
			let strikes = self.increment_strike(peer);
			if strikes >= MAX_TIMEOUT_STRIKES {
				warn!(
					peer = %peer,
					strikes,
					"peer reached max timeout strikes from header timeouts, excluding from requests"
				);
			}
		}

		// Check if all connected peers are excluded
		if !timed_out_peers.is_empty() {
			let all_excluded = self.nodes.iter().all(|entry| {
				let node = entry.value();
				!node.state.is_connected() || node.strike_excluded
			});
			if all_excluded && self.connected_count() > 0 {
				warn!("all connected peers are excluded from header requests");
			}
		}
	}

	/// Background loop that scans for header request timeouts every 3 seconds
	pub async fn run_header_timeout_scanner(self: Arc<Self>) {
		loop {
			tokio::time::sleep(Duration::from_secs(3)).await;
			self.scan_header_timeouts();
		}
	}

	/// Returns the current chain tip height (lock-free, from atomic cache)
	pub fn chain_height(&self) -> u32 {
		self.cached_height.load(Ordering::Relaxed)
	}

	/// Returns the compact target (nBits) of the current chain tip (lock-free, from atomic cache)
	pub fn chain_tip_bits(&self) -> u32 {
		self.cached_tip_bits.load(Ordering::Relaxed)
	}

	/// Refreshes the cached height and `tip_bits` from the header store
	///
	/// Must be called after any write to `header_store` that changes the tip
	pub fn refresh_chain_cache(&self) {
		let store = self.header_store.read();
		self.cached_height.store(store.height(), Ordering::Relaxed);
		self.cached_tip_bits.store(store.tip_bits(), Ordering::Relaxed);
	}

	/// Returns true if the node is in initial block download
	///
	/// IBD is defined as the chainstate tip being more than 1000 blocks
	/// behind the header tip. During IBD, reorg attempts are suppressed
	pub fn is_in_ibd(&self) -> bool {
		let header_height = self.header_store.read().height();
		let chain_height = self.chainstate().map_or(0, |cs| cs.tip_height());
		header_height.saturating_sub(chain_height) > 1_000
	}

	/// Attempts to activate the best known chain if it differs from the chainstate tip
	///
	/// Acquires write locks on header store and mempool, then calls the
	/// reorg coordinator. Returns the result for the caller to act on
	/// (e.g. resetting the block download manager). No-op during IBD
	#[allow(clippy::significant_drop_tightening)] // headers write lock intentionally held through reorg + purge
	pub fn try_activate_best_chain(&self) -> Option<crate::reorg::ActivateResult> {
		if self.is_in_ibd() {
			return None;
		}

		let chainstate = self.chainstate()?;
		let block_store = self.block_store()?;
		let mempool_arc = self.mempool()?;

		// Quick check: does best_tip differ from chainstate tip?
		let (best_hash, _, _) = self.header_store.read().best_tip();
		if best_hash == chainstate.tip() {
			return None;
		}

		let mut headers = self.header_store.write();
		// Try to acquire mempool lock without blocking. If contended,
		// skip this attempt -- we'll retry on the next header message.
		// This avoids the tokio anti-pattern of calling blocking_write
		// on an async RwLock from a synchronous spawn_blocking context
		let Ok(mut mempool) = mempool_arc.try_write() else {
			warn!("mempool lock contended during reorg attempt, will retry");
			return None;
		};

		match crate::reorg::activate_best_chain(&mut headers, &chainstate, &block_store, &mut mempool) {
			Ok(result) => {
				if let crate::reorg::ActivateResult::Reorganized {
					ref old_tip,
					ref new_tip,
					disconnected,
					connected,
				} = result
				{
					let new_height = chainstate.tip_height();
					info!(
						old_tip = %old_tip,
						new_tip = %new_tip,
						disconnected,
						connected,
						"chain reorganization completed"
					);

					// Notify download manager to reset its state
					if let Some(sender) = self.reorg_sender.lock().as_ref() {
						let _ = sender.send(new_height);
					}

					// Purge stale fork headers
					headers.purge_stale_forks(new_height);
				}
				Some(result)
			}
			Err(e) => {
				error!(error = %e, "chain reorganization failed");
				None
			}
		}
	}

	/// Returns stats and node snapshots in a single `DashMap` iteration
	///
	/// Only acquires shard locks once for both stats and snapshot data
	pub fn get_snapshot(&self) -> (NodeStats, Vec<NodeSnapshot>) {
		let mut connected: usize = 0;
		let mut incoming: usize = 0;
		let mut outgoing: usize = 0;
		let mut handshaking: usize = 0;
		let mut connecting: usize = 0;
		let mut disconnected: usize = 0;
		let mut dead: usize = 0;
		let mut banned: usize = 0;

		let snapshots: Vec<NodeSnapshot> = self
			.nodes
			.iter()
			.map(|entry| {
				let node = entry.value();
				match &node.state {
					NodeState::Connected { .. } => {
						connected = connected.saturating_add(1);
						match node.connection_type {
							ConnectionType::Incoming => {
								incoming = incoming.saturating_add(1);
							}
							ConnectionType::Outgoing => {
								outgoing = outgoing.saturating_add(1);
							}
						}
					}
					NodeState::Handshaking { .. } => {
						handshaking = handshaking.saturating_add(1);
					}
					NodeState::Connecting { .. } => {
						connecting = connecting.saturating_add(1);
					}
					NodeState::Disconnected { .. } => {
						disconnected = disconnected.saturating_add(1);
					}
					NodeState::Dead => {
						dead = dead.saturating_add(1);
					}
					NodeState::Banned { .. } => {
						banned = banned.saturating_add(1);
					}
				}
				NodeSnapshot {
					address: *entry.key(),
					port: node.port,
					height: node.height,
					connection_type: node.connection_type,
					state_label: NodeStateLabel::from_state(&node.state),
					version: node.version,
					user_agent: node.user_agent.clone(),
					last_seen: node.last_seen,
					timeout_strikes: node.timeout_strikes,
					strike_excluded: node.strike_excluded,
				}
			})
			.collect();

		let total = snapshots.len();

		(
			NodeStats {
				total,
				connected,
				incoming,
				outgoing,
				handshaking,
				connecting,
				disconnected,
				dead,
				banned,
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
				info!(count = ready_nodes.len(), "reaper: nodes ready for retry");
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
				warn!(peer = %address, "node stuck in stale state, scheduling retry");
				if let Some(mut node) = self.nodes.get_mut(&address)
					&& matches!(node.state, NodeState::Connecting { .. } | NodeState::Handshaking { .. })
				{
					node.inv_known.clear();
					node.state = NodeState::Disconnected {
						retry_at: Instant::now(),
						attempt: 0,
					};
				}
			}

			if !expired_bans.is_empty() {
				info!(count = expired_bans.len(), "Reaper: clearing expired bans");
			}
			for address in expired_bans {
				if let Some(mut node) = self.nodes.get_mut(&address)
					&& matches!(node.state, NodeState::Banned { .. })
					&& !node.state.is_banned()
				{
					node.state = NodeState::Dead;
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
				total,
				connected,
				disconnected = total.saturating_sub(connected),
				"reaper scan complete"
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

	/// Returns writers for all currently connected peers
	///
	/// Collects results without holding `DashMap` locks across the iteration
	pub fn get_connected_writers(&self) -> Vec<(IpAddr, SharedTcpWriter)> {
		self.nodes
			.iter()
			.filter_map(|entry| {
				if let NodeState::Connected { ref writer } = entry.value().state {
					Some((*entry.key(), Arc::clone(writer)))
				} else {
					None
				}
			})
			.collect()
	}

	/// Returns true if a peer already knows about an inventory hash
	pub fn peer_knows_inv(&self, peer: &IpAddr, hash: &Hash256) -> bool {
		self.nodes.get(peer).is_some_and(|n| n.inv_known.contains(hash))
	}

	/// Marks an inventory hash as known to a peer
	pub fn mark_peer_inv_known(&self, peer: &IpAddr, hash: Hash256) {
		if let Some(mut node) = self.nodes.get_mut(peer) {
			if node.inv_known.len() >= MAX_INV_KNOWN {
				node.inv_known.clear();
			}
			node.inv_known.insert(hash);
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

	/// Returns true if our listen port has been confirmed reachable from the outside
	pub fn is_port_reachable(&self) -> bool {
		self.port_reachable.load(Ordering::Acquire)
	}

	/// Probes our own external address to check if our listen port is reachable.
	/// Attempts a TCP connect to `external_ip:listen_port` with a short timeout.
	/// If the connection succeeds, we are publicly reachable and can safely
	/// advertise our address without polluting peer tables with unreachable entries
	pub async fn probe_reachability(&self) {
		let Some(external_ip) = self.get_external_ip() else {
			return;
		};

		let addr = std::net::SocketAddr::new(external_ip, self.listen_port);

		match timeout(REACHABILITY_PROBE_TIMEOUT, TcpStream::connect(addr)).await {
			Ok(Ok(stream)) => {
				// Connection succeeded, port is reachable
				drop(stream);
				if !self.port_reachable.swap(true, Ordering::Release) {
					info!(
						address = %external_ip,
						port = self.listen_port,
						"port reachability confirmed via self-probe"
					);
				}
			}
			Ok(Err(e)) => {
				self.port_reachable.store(false, Ordering::Release);
				debug!(
					address = %external_ip,
					port = self.listen_port,
					error = %e,
					"port reachability probe failed"
				);
			}
			Err(_) => {
				self.port_reachable.store(false, Ordering::Release);
				debug!(
					address = %external_ip,
					port = self.listen_port,
					"port reachability probe timed out"
				);
			}
		}
	}

	/// Revives a Dead node if the given timestamp is newer than `last_seen`
	///
	/// Returns true if the node was revived, false otherwise
	pub fn revive_if_newer(self: &Arc<Self>, address: &IpAddr, port: u16, timestamp: u32) -> bool {
		if let Some(mut node) = self.nodes.get_mut(address)
			&& matches!(node.state, NodeState::Dead)
			&& u64::from(timestamp) > node.last_seen
		{
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
		false
	}

	/// Periodically announces our own address to all connected peers
	///
	/// Re-probes reachability each cycle. Only announces if our port is
	/// confirmed reachable to avoid polluting peer tables with NAT'd addresses
	pub async fn run_self_announce(self: Arc<Self>) {
		loop {
			tokio::time::sleep(SELF_ANNOUNCE_INTERVAL).await;

			let Some(external_ip) = self.get_external_ip() else {
				debug!("Self-announce skipped: no consensus on external IP");
				continue;
			};

			// Re-probe each cycle in case network conditions changed
			self.probe_reachability().await;

			if !self.is_port_reachable() {
				debug!("Self-announce skipped: port not reachable");
				continue;
			}

			let addr = NetworkAddress::new(external_ip, self.listen_port);
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
				address = %external_ip,
				port = self.listen_port,
				peer_count = writers.len(),
				"announcing own address to peers"
			);

			for (address, writer) in &writers {
				if let Err(e) = writer.send_message("addr", &payload).await {
					debug!(peer = %address, error = %e, "failed to self-announce");
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
					&& *expires > now
				{
					return Some(crate::storage::bans::SavedBan {
						ip: *entry.key(),
						reason: reason.to_string(),
						created: *created,
						expires: *expires,
					});
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
	node_manager.notify_peer_disconnected(address);

	let next_attempt = attempt.saturating_add(1);
	if next_attempt >= MAX_RETRY_ATTEMPTS {
		info!(peer = %address, "node exhausted all retry attempts, marking dead");
		node_manager.set_state(address, NodeState::Dead);
	} else {
		let backoff = calculate_backoff(next_attempt);
		debug!(peer = %address, attempt = next_attempt, delay = ?backoff, "scheduling retry");
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
		trace!(peer = %address, port, "avoiding connection, not a good candidate");
		return;
	}

	// Attempt to establish a TCP connection with retries
	let mut tcp_stream = None;

	for tcp_attempt in 1..=TCP_MAX_ATTEMPTS {
		let fail_reason = match timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((address, port))).await {
			Ok(Ok(stream)) => {
				info!(peer = %address, port, attempt = tcp_attempt, "TCP connected");
				tcp_stream = Some(stream);
				break;
			}
			Ok(Err(e)) => format!("{e}"),
			Err(_) => "timed out".to_string(),
		};

		if tcp_attempt < TCP_MAX_ATTEMPTS {
			trace!(peer = %address, port, attempt = tcp_attempt, reason = %fail_reason, "TCP connect failed, retrying");
			sleep(TCP_RETRY_DELAY).await;
		} else {
			debug!(peer = %address, port, attempts = TCP_MAX_ATTEMPTS, reason = %fail_reason, "TCP connect failed after all attempts");
			schedule_retry(&node_manager, &address, attempt);
			return;
		}
	}

	let Some(mut tcp_stream) = tcp_stream else {
		error!(peer = %address, port, attempts = TCP_MAX_ATTEMPTS, "failed to establish connection");
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
	let tip_height = node_manager.chainstate().map_or(0, |cs| cs.tip_height());
	let version = MessageVersion::new(network_address, node_manager.my_nonce, tip_height);
	let packet = match Message::new("version", &version.to_bytes()) {
		Ok(p) => p,
		Err(e) => {
			error!(peer = %address, port, error = %e, "failed to create version message");
			schedule_retry(&node_manager, &address, attempt);
			return;
		}
	};

	if let Err(e) = tcp_stream.write_all(&packet.to_bytes()).await {
		error!(peer = %address, port, error = %e, "failed to send version");
		schedule_retry(&node_manager, &address, attempt);
		return;
	}

	// Hand off the connection to the connection loop
	node_connection_loop(Arc::clone(&node_manager), address, tcp_stream).await;

	// Don't overwrite banned nodes -- they were banned for a reason
	if let Some(node) = node_manager.nodes.get(&address)
		&& node.state.is_banned()
	{
		debug!(peer = %address, "node is banned, not scheduling retry");
		return;
	}

	// Connection loop ended -- schedule retry
	schedule_retry(&node_manager, &address, attempt);

	debug!(peer = %address, port, "exiting handle_node_connection");
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
							warn!(peer = %address, error = %e, "error processing incoming data");
							break;
						}
					}
					Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
						continue; // No data available, try again
					}
					Err(e) => {
						warn!(error = ?e, "error in try_read()");
						break;
					}
				}
			}
			_ = ping_interval.tick() => {
				node_manager.send_ping(&address).await;
			}
			() = tokio::time::sleep_until(idle_deadline) => {
				warn!(peer = %address, timeout = ?IDLE_TIMEOUT, "connection idle, disconnecting");
				break;
			}
		}

		// Process all messages in the queue
		while let Some(message) = incoming_queue.get_next_message() {
			node_manager.update_last_seen(&address);

			if let Err(e) = parse_incoming_message(&node_manager, &address, &shared_writer, message).await {
				warn!(peer = %address, error = ?e, "error parsing message");

				// Close the connection
				if let Err(e) = shared_writer.lock().await.shutdown().await {
					warn!(peer = %address, error = %e, "error shutting down writer");
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
		warn!(peer = %address, "malformed 'command' field in message");
		return Err(anyhow!("Error parsing incoming message, 'command' field is malformed"));
	};

	let command = NetworkCommand::from_command_str(command);

	debug!(peer = %address, command = ?command, "received message");

	// Only allow Version/Verack before the handshake completes
	if !matches!(command, NetworkCommand::Version | NetworkCommand::Verack) {
		let is_connected = node_manager.nodes.get(address).is_some_and(|n| n.state.is_connected());
		if !is_connected {
			warn!(peer = %address, command = ?command, "received message before handshake complete, ignoring");
			return Ok(());
		}
	}

	match command {
		NetworkCommand::Version => {
			handler_version::handle_version(node_manager, address, tcp_writer, &message.payload).await
		}
		NetworkCommand::Verack => handler_verack::handle_verack(node_manager, address, tcp_writer).await,
		NetworkCommand::Ping => handler_ping::handle_ping(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::Pong => {
			handler_ping::handle_pong(node_manager, address, &message.payload);
			Ok(())
		}
		NetworkCommand::Addr => handler_addr::handle_addr(node_manager, address, &message.payload).await,
		NetworkCommand::Alert => {
			debug!(peer = %address, "received alert, ignoring");
			Ok(())
		}
		NetworkCommand::GetAddr => handler_addr::handle_getaddr(node_manager, address, tcp_writer).await,
		NetworkCommand::Inv => handler_inv::handle_inv(node_manager, address, tcp_writer, &message.payload).await,
		NetworkCommand::GetData => {
			handler_inv::handle_getdata(node_manager, address, tcp_writer, &message.payload).await
		}
		NetworkCommand::NotFound => {
			handler_inv::handle_notfound(address, &message.payload);
			Ok(())
		}
		NetworkCommand::Headers => {
			handler_headers::handle_headers(node_manager, address, tcp_writer, &message.payload).await
		}
		NetworkCommand::GetHeaders => {
			handler_headers::handle_getheaders(node_manager, address, tcp_writer, &message.payload).await
		}
		NetworkCommand::SendHeaders => {
			handler_headers::handle_sendheaders(node_manager, address);
			Ok(())
		}
		NetworkCommand::Block => handler_block::handle_block(node_manager, address, &message.payload),
		NetworkCommand::Tx => {
			let Some(mempool) = node_manager.mempool() else {
				debug!(peer = %address, "received tx but mempool not yet attached, ignoring");
				return Ok(());
			};
			let Some(chainstate) = node_manager.chainstate() else {
				debug!(peer = %address, "received tx but chainstate not yet attached, ignoring");
				return Ok(());
			};
			handler_tx::handle_tx(
				node_manager,
				address,
				tcp_writer,
				&message.payload,
				&mempool,
				&chainstate,
			)
			.await
		}
		NetworkCommand::GetBlocks => {
			handler_getblocks::handle_getblocks(node_manager, address, tcp_writer, &message.payload).await
		}
		NetworkCommand::Mempool => handler_mempool::handle_mempool(node_manager, address, tcp_writer).await,
		NetworkCommand::Unknown(cmd) => {
			warn!(peer = %address, command = %cmd, "unknown command");
			Ok(())
		}
	}
}
