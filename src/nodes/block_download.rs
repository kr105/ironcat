// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::chainstate::ChainState;
use crate::network::message_inv::{InvItem, InvType, MessageInv};
use crate::network::{SharedTcpWriter, SharedTcpWriterExt};
use crate::nodes::NodeManager;
use crate::storage::block_store::BlockStore;
use crate::types::block::Block;
use crate::types::hash::Hash256;

/// Maximum blocks in flight across all peers
const WINDOW_SIZE: usize = 512;

/// Maximum blocks in flight per individual peer
const PER_PEER_LIMIT: usize = 48;

/// How long before a request is considered stale and reassigned
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to scan for timed-out requests when blocks are in flight
const SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// How often to poll when idle (waiting for headers to sync)
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Flush block store to disk every N stored blocks
const FLUSH_INTERVAL: u32 = 500;

/// Log block download progress every N stored blocks
const PROGRESS_LOG_INTERVAL: u32 = 500;

/// How many heights to fetch from the header store per batch during
/// `fill_window` scanning. Larger means fewer `RwLock` acquires when
/// skipping past already-stored blocks
const SCAN_BATCH_SIZE: u32 = 4096;

/// Only refill the window when at least this many slots are free.
/// Prevents micro-batches (getdata count=1) by accumulating free slots
/// before issuing requests, producing larger batches per peer
const REFILL_THRESHOLD: usize = WINDOW_SIZE / 4;

/// Maximum number of out-of-order blocks to buffer before discarding.
/// Bounds memory usage when blocks arrive far ahead of `next_connect_height`
const MAX_PENDING_BLOCKS: usize = WINDOW_SIZE * 2;

/// Sliding-window block download manager
///
/// Requests blocks by hash from connected peers, validates merkle roots,
/// and writes raw block data to flat files on disk.
///
/// Performance design:
/// - In-memory `HashSet` tracks stored blocks (no redb reads in hot path)
/// - Batch header lookups via `hashes_in_range` (one `RwLock` acquire per fill)
/// - Refill threshold prevents micro-batches (waits for 25% window free)
pub struct BlockDownloadManager {
	node_manager: Arc<NodeManager>,
	block_store: Arc<BlockStore>,
	chainstate: Arc<ChainState>,
	block_rx: mpsc::Receiver<(Hash256, Vec<u8>)>,
	/// Blocks currently being downloaded, keyed by block hash
	pub in_flight: HashMap<Hash256, InFlightEntry>,
	/// Next chain height to request
	pub next_height: u32,
	/// In-memory set of all stored block hashes (avoids redb reads)
	stored_hashes: HashSet<Hash256>,
	/// Blocks waiting to be connected in order, keyed by height
	pending_blocks: BTreeMap<u32, (Hash256, Block, Vec<u8>)>,
	/// Next height that needs to be connected to the chainstate
	next_connect_height: u32,
	/// Whether we were caught up on the previous loop iteration
	was_caught_up: bool,
	/// Blocks stored since last flush
	blocks_since_flush: u32,
	/// Blocks stored since last progress log
	blocks_since_progress_log: u32,
}

/// Tracks a single in-flight block request
pub struct InFlightEntry {
	/// Peer that was sent the getdata
	pub peer: IpAddr,
	/// When the request was sent
	pub requested_at: Instant,
	/// Chain height of the requested block
	pub height: u32,
}

impl BlockDownloadManager {
	/// Creates a new download manager starting from height 1
	///
	/// Loads all existing block hashes from the store index into memory
	/// so that `fill_window` can skip stored blocks with O(1) `HashSet`
	/// lookups instead of per-height redb read transactions.
	/// Genesis block (height 0) is skipped since it has no meaningful
	/// transactions to download.
	/// Initializes `next_connect_height` from the chainstate tip so
	/// already-connected blocks are not re-processed
	pub fn new(
		node_manager: Arc<NodeManager>,
		block_store: Arc<BlockStore>,
		block_rx: mpsc::Receiver<(Hash256, Vec<u8>)>,
		chainstate: Arc<ChainState>,
	) -> Self {
		let stored_hashes = match block_store.all_indexed_hashes() {
			Ok(hashes) => hashes,
			Err(e) => {
				warn!(error = %e, "failed to load block index hashes, will re-download stored blocks");
				HashSet::new()
			}
		};

		// Initialize cached block height so the TUI shows progress immediately
		#[allow(clippy::cast_possible_truncation)] // stored blocks won't exceed u32
		node_manager.set_block_height(stored_hashes.len() as u32);

		// Start connecting from one past the chainstate tip (tip=0 means empty, start at 1)
		let cs_height = chainstate.tip_height();
		let next_connect_height = cs_height.saturating_add(1);

		info!(
			stored_blocks = stored_hashes.len(),
			chainstate_tip_height = cs_height,
			next_connect_height,
			"BlockDownloadManager initialized"
		);

		Self {
			node_manager,
			block_store,
			chainstate,
			block_rx,
			in_flight: HashMap::new(),
			next_height: 1,
			stored_hashes,
			pending_blocks: BTreeMap::new(),
			next_connect_height,
			was_caught_up: false,
			blocks_since_flush: 0,
			blocks_since_progress_log: 0,
		}
	}

	/// Connects stored-but-unconnected blocks from disk to the chainstate
	///
	/// On restart the block store may be ahead of the chainstate. Instead
	/// of re-downloading those blocks from peers, reads them from the flat
	/// files and connects them in order
	fn catch_up_chainstate(&mut self) {
		let chain_height = self.node_manager.chain_height();
		let mut connected: u32 = 0;

		'outer: while self.next_connect_height <= chain_height {
			let range_end = self
				.next_connect_height
				.saturating_add(SCAN_BATCH_SIZE)
				.min(chain_height.saturating_add(1));
			let height_hashes = self
				.node_manager
				.header_store
				.read()
				.hashes_in_range(self.next_connect_height, range_end);

			if height_hashes.is_empty() {
				break;
			}

			for (height, hash) in &height_hashes {
				if !self.stored_hashes.contains(hash) {
					break 'outer;
				}

				let raw_bytes = match self.block_store.load_block(hash) {
					Ok(bytes) => bytes,
					Err(e) => {
						error!(block = %hash, height, error = %e, "failed to load block from store for chainstate catch-up");
						break 'outer;
					}
				};

				let block = match Block::from_bytes_and_validate(&raw_bytes) {
					Ok(b) => b,
					Err(e) => {
						error!(block = %hash, height, error = %e, "failed to parse stored block for chainstate catch-up");
						break 'outer;
					}
				};

				if let Err(e) = self.chainstate.connect_block(&block, *height, hash) {
					error!(block = %hash, height, error = %e, "failed to connect stored block to chainstate");
					break 'outer;
				}

				self.next_connect_height = height.saturating_add(1);
				connected = connected.saturating_add(1);

				if connected.is_multiple_of(FLUSH_INTERVAL) {
					self.flush_store();
				}
			}
		}

		if connected > 0 {
			self.flush_store();
			info!(
				connected,
				tip_height = self.next_connect_height.saturating_sub(1),
				"chainstate catch-up from disk complete"
			);
		}
	}

	/// Main download loop
	///
	/// Continuously fills the request window, drains all available blocks
	/// from the channel, and scans for timed-out requests until the
	/// channel is closed
	pub async fn run(&mut self) {
		self.catch_up_chainstate();

		loop {
			self.fill_window().await;

			let caught_up = self.is_caught_up();
			if caught_up && !self.was_caught_up {
				self.flush_store();
				info!(
					height = self.next_height.saturating_sub(1),
					"Block download caught up to header tip"
				);
			}
			self.was_caught_up = caught_up;

			// Use short poll when idle (no in-flight requests), long interval otherwise
			let sleep_duration = if self.in_flight.is_empty() {
				IDLE_POLL
			} else {
				SCAN_INTERVAL
			};

			tokio::select! {
				result = self.block_rx.recv() => {
					let Some((hash, raw_bytes)) = result else {
						self.flush_store();
						info!("block channel closed, shutting down download manager");
						break;
					};
					self.handle_received_block(hash, &raw_bytes);

					// Drain all remaining buffered blocks without waiting
					while let Ok((hash, raw_bytes)) = self.block_rx.try_recv() {
						self.handle_received_block(hash, &raw_bytes);
					}
				}
				() = tokio::time::sleep(sleep_duration) => {
					// Flush any pending blocks on the timer tick
					if self.blocks_since_flush > 0 {
						self.flush_store();
					}
				}
			}

			// Always scan for stale requests after any event, not just on
			// the sleep branch. Without this, continuous block arrivals starve
			// the timeout scanner and stalled peers block progress forever
			self.handle_timeout_scan();
		}
	}

	/// Returns true when all requested blocks are received and we are past the header tip
	pub fn is_caught_up(&self) -> bool {
		self.in_flight.is_empty() && self.next_height > self.node_manager.chain_height()
	}

	/// Fills empty window slots by requesting blocks from connected peers
	///
	/// Uses batch header lookups (one `RwLock` acquire per batch) and
	/// in-memory hash checks (no redb reads). Only runs when enough slots
	/// are free to produce meaningful batch sizes per peer. Scans past
	/// already-stored blocks in a tight loop since `stored_hashes` lookups
	/// are O(1) `HashSet` operations
	#[allow(clippy::too_many_lines)] // three sequential phases (collect, assign, send) that share local state
	async fn fill_window(&mut self) {
		// Every in-flight block can become a pending block when it arrives
		// out of order. Cap new requests so that pending + in_flight + new
		// never exceeds MAX_PENDING_BLOCKS, preventing any discards
		let total_outstanding = self.pending_blocks.len().saturating_add(self.in_flight.len());
		let pending_headroom = MAX_PENDING_BLOCKS.saturating_sub(total_outstanding);
		let in_flight_headroom = WINDOW_SIZE.saturating_sub(self.in_flight.len());
		let free_slots = in_flight_headroom.min(pending_headroom);

		// Don't bother refilling for tiny batches -- wait until enough slots
		// are free to produce meaningful getdata messages.
		// Exception: when window is completely empty (startup or after timeout
		// scan clears everything), always fill immediately
		if free_slots < REFILL_THRESHOLD && !self.in_flight.is_empty() {
			return;
		}

		let chain_height = self.node_manager.chain_height();
		let writers = self.get_connected_writers();

		if writers.is_empty() || self.next_height > chain_height {
			return;
		}

		// --- Phase 1: Collect all requestable (height, hash) pairs ---
		let mut requestable: Vec<(u32, Hash256)> = Vec::new();

		while requestable.len() < free_slots && self.next_height <= chain_height {
			let range_end = self
				.next_height
				.saturating_add(SCAN_BATCH_SIZE)
				.min(chain_height.saturating_add(1));
			let height_hashes = self
				.node_manager
				.header_store
				.read()
				.hashes_in_range(self.next_height, range_end);

			if height_hashes.is_empty() {
				break;
			}

			let mut made_progress = false;
			for (height, hash) in &height_hashes {
				if requestable.len() >= free_slots {
					break;
				}

				self.next_height = self.next_height.max(height.saturating_add(1));
				made_progress = true;

				// Skip blocks we already have, are in flight, or are pending connection
				if self.stored_hashes.contains(hash)
					|| self.in_flight.contains_key(hash)
					|| self.pending_blocks.contains_key(height)
				{
					continue;
				}

				requestable.push((*height, *hash));
			}

			if !made_progress {
				break;
			}
		}

		if requestable.is_empty() {
			return;
		}

		// --- Phase 2: Assign contiguous chunks to peers ---
		// Compute how many slots each peer has available
		let mut peer_used: HashMap<IpAddr, usize> = HashMap::new();
		for entry in self.in_flight.values() {
			#[allow(clippy::arithmetic_side_effects)] // bounded by WINDOW_SIZE
			{
				*peer_used.entry(entry.peer).or_insert(0) += 1;
			}
		}

		// Build ordered list of (peer_index, ip, available_slots)
		let mut available_peers: Vec<(usize, IpAddr, usize)> = writers
			.iter()
			.enumerate()
			.map(|(i, (ip, _))| {
				let used = peer_used.get(ip).copied().unwrap_or(0);
				let free = PER_PEER_LIMIT.saturating_sub(used);
				(i, *ip, free)
			})
			.filter(|(_, _, free)| *free > 0)
			.collect();

		if available_peers.is_empty() {
			debug!("all peers at per-peer limit, pausing fill_window");
			return;
		}

		// Assign contiguous chunks: fill each peer to its limit before moving
		// to the next. A slow peer only blocks its own height range
		let mut peer_requests: HashMap<usize, Vec<InvItem>> = HashMap::new();
		let mut new_entries: Vec<(Hash256, InFlightEntry, usize)> = Vec::new();
		let mut peer_idx = 0;

		for (height, hash) in &requestable {
			// Advance to next peer if current one is full
			// peer_idx < len is checked by the condition; indexing is safe
			#[allow(clippy::indexing_slicing)]
			while peer_idx < available_peers.len() && available_peers[peer_idx].2 == 0 {
				peer_idx = peer_idx.saturating_add(1);
			}
			if peer_idx >= available_peers.len() {
				break;
			}

			// available_peers[peer_idx] is valid (checked above)
			#[allow(clippy::indexing_slicing)]
			let (widx, peer_ip, ref mut remaining) = available_peers[peer_idx];

			peer_requests.entry(widx).or_default().push(InvItem {
				inv_type: InvType::Block,
				hash: *hash,
			});

			new_entries.push((
				*hash,
				InFlightEntry {
					peer: peer_ip,
					requested_at: Instant::now(),
					height: *height,
				},
				widx,
			));

			#[allow(clippy::arithmetic_side_effects)] // remaining > 0 checked by while loop
			{
				*remaining -= 1;
			}
		}

		if new_entries.is_empty() {
			return;
		}

		// --- Phase 3: Send getdata messages (one per peer) ---
		let mut failed_min_height: Option<u32> = None;

		for (widx, items) in &peer_requests {
			// widx comes from enumerate() on writers above
			#[allow(clippy::indexing_slicing)]
			let (peer_ip, ref writer) = writers[*widx];

			let count = items.len();
			let msg = MessageInv::new(items.clone());
			if let Err(e) = writer
				.send_message("getdata", &msg.to_bytes())
				.await
				.context("failed to send batched getdata")
			{
				warn!(peer = %peer_ip, count, error = %e, "failed to send batched getdata");
				// Track minimum height of dropped entries so we can re-request them
				for entry in new_entries.iter().filter(|(_, _, w)| *w == *widx) {
					let h = entry.1.height;
					failed_min_height = Some(failed_min_height.map_or(h, |m: u32| m.min(h)));
				}
				new_entries.retain(|(_h, _e, w)| *w != *widx);
				continue;
			}

			debug!(peer = %peer_ip, count, "sent batched getdata");
		}

		// Commit successful entries to in_flight
		for (hash, entry, _widx) in new_entries {
			self.in_flight.insert(hash, entry);
		}

		// Re-request blocks whose getdata send failed
		if let Some(min_height) = failed_min_height {
			self.schedule_redownload(min_height);
		}
	}

	/// Processes a received block: validates structure, context, and stores to disk
	///
	/// On any validation or connection failure the block is scheduled for
	/// re-download from a different peer via `schedule_redownload`
	#[allow(clippy::too_many_lines)] // sequential validation stages with error handling at each step
	fn handle_received_block(&mut self, hash: Hash256, raw_bytes: &[u8]) {
		let Some(entry) = self.in_flight.remove(&hash) else {
			debug!(block = %hash, "received block not in flight, ignoring");
			return;
		};

		// Parse and validate merkle root
		let block = match Block::from_bytes_and_validate(raw_bytes) {
			Ok(block) => block,
			Err(e) => {
				warn!(
					block = %hash,
					height = entry.height,
					peer = %entry.peer,
					error = %e,
					"block parsing/merkle validation failed, will re-request"
				);
				self.schedule_redownload(entry.height);
				return;
			}
		};

		// Structural validation (coinbase, weight, tx checks)
		if let Err(e) = crate::validation::block::check_block(&block) {
			warn!(
				block = %hash,
				height = entry.height,
				peer = %entry.peer,
				error = %e,
				"block structural validation failed, will re-request"
			);
			self.schedule_redownload(entry.height);
			return;
		}

		// Contextual validation (timestamp, BIP34, subsidy)
		if entry.height > 0 {
			let mtp = {
				let headers = self.node_manager.header_store.read();
				#[allow(clippy::arithmetic_side_effects)] // entry.height > 0 checked above
				crate::validation::compute_median_time_past(&*headers, entry.height - 1)
			};

			let Some(mtp) = mtp else {
				warn!(
					block = %hash,
					height = entry.height,
					"could not compute MTP for contextual validation, will re-request"
				);
				self.schedule_redownload(entry.height);
				return;
			};

			// Use system time as adjusted time (peer-based offset is a future refinement)
			let adjusted_time = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
				Ok(d) => {
					#[allow(clippy::cast_possible_truncation)] // Unix timestamp fits u32 until 2106
					let secs = d.as_secs() as u32;
					secs
				}
				Err(e) => {
					warn!(error = %e, "system clock is before Unix epoch, will re-request");
					self.schedule_redownload(entry.height);
					return;
				}
			};

			if let Err(e) = crate::validation::check_block_context(&block, entry.height, mtp, adjusted_time) {
				warn!(
					block = %hash,
					height = entry.height,
					peer = %entry.peer,
					error = %e,
					"block contextual validation failed, will re-request"
				);
				self.schedule_redownload(entry.height);
				return;
			}
		}

		// Connect block to chainstate if it's at the expected height
		match entry.height.cmp(&self.next_connect_height) {
			std::cmp::Ordering::Equal => {
				if self.connect_and_store(hash, &block, raw_bytes, entry.height) {
					self.drain_pending();
				} else {
					self.schedule_redownload(entry.height);
				}
			}
			std::cmp::Ordering::Greater => {
				// Out of order: buffer for later if we haven't hit the cap
				if self.pending_blocks.len() >= MAX_PENDING_BLOCKS {
					warn!(
						block = %hash,
						height = entry.height,
						pending = self.pending_blocks.len(),
						"pending buffer full, discarding out-of-order block"
					);
					self.schedule_redownload(entry.height);
				} else {
					self.pending_blocks
						.insert(entry.height, (hash, block, raw_bytes.to_vec()));
					debug!(
						block = %hash,
						height = entry.height,
						next_connect = self.next_connect_height,
						pending = self.pending_blocks.len(),
						"block buffered for ordered connection"
					);
				}
			}
			std::cmp::Ordering::Less => {
				// Already connected, just store the raw data
				self.store_block_data(&hash, raw_bytes, entry.height);
			}
		}
	}

	/// Resets `next_height` so `fill_window` will re-request a failed block
	///
	/// The block was removed from `in_flight` but never stored. Without
	/// this, `next_height` has already advanced past it and the block
	/// would never be requested again
	fn schedule_redownload(&mut self, height: u32) {
		self.next_height = self.next_height.min(height);
	}

	/// Connects a block to the chainstate and stores it to disk
	///
	/// On connect failure the block is discarded and returns `false`.
	/// On success the raw data is written to the block store, progress
	/// counters are updated, and returns `true`
	fn connect_and_store(&mut self, hash: Hash256, block: &Block, raw_bytes: &[u8], height: u32) -> bool {
		if let Err(e) = self.chainstate.connect_block(block, height, &hash) {
			warn!(
				block = %hash,
				height,
				error = %e,
				"failed to connect block to chainstate, discarding"
			);
			return false;
		}

		self.store_block_data(&hash, raw_bytes, height);
		// Advance to the next height after successful connection
		self.next_connect_height = height.saturating_add(1);
		true
	}

	/// Drains buffered pending blocks that are now consecutive
	///
	/// After a successful `connect_and_store`, checks whether the
	/// pending buffer contains the next expected height and keeps
	/// connecting until there is a gap or the buffer is empty.
	/// Note: `connect_and_store` advances `next_connect_height` on
	/// success, so each iteration's `remove` targets the next key
	fn drain_pending(&mut self) {
		while let Some(entry) = self.pending_blocks.remove(&self.next_connect_height) {
			let (hash, block, raw_bytes) = entry;
			// Stop draining on failure -- subsequent blocks depend on this one
			if !self.connect_and_store(hash, &block, &raw_bytes, self.next_connect_height) {
				self.schedule_redownload(self.next_connect_height);
				return;
			}
		}
	}

	/// Stores raw block data and updates progress counters
	///
	/// Skips the flat file write if the block is already in the store
	fn store_block_data(&mut self, hash: &Hash256, raw_bytes: &[u8], height: u32) {
		if !self.stored_hashes.insert(*hash) {
			// Already stored, nothing to do
			debug!(block = %hash, height, "block already stored, skipping");
			return;
		}

		if let Err(e) = self.block_store.store_block_unchecked(hash, raw_bytes) {
			warn!(
				block = %hash,
				height,
				error = %e,
				"failed to store block"
			);
			return;
		}

		self.blocks_since_flush = self.blocks_since_flush.saturating_add(1);

		// Batch flush: fsync + redb commit every FLUSH_INTERVAL blocks
		if self.blocks_since_flush >= FLUSH_INTERVAL {
			self.flush_store();
		}

		// Report actual stored count (not next_height which is the next height to request)
		#[allow(clippy::cast_possible_truncation)] // stored blocks won't exceed u32
		let stored_count = self.stored_hashes.len() as u32;
		self.node_manager.set_block_height(stored_count);

		self.blocks_since_progress_log = self.blocks_since_progress_log.saturating_add(1);
		if self.blocks_since_progress_log >= PROGRESS_LOG_INTERVAL {
			let chain_height = self.node_manager.chain_height();
			info!(
				stored = stored_count,
				chain_height,
				in_flight = self.in_flight.len(),
				"block download progress"
			);
			self.blocks_since_progress_log = 0;
		}

		debug!(block = %hash, height, "block stored");
	}

	/// Flushes the block store and undo store, then resets the counter
	fn flush_store(&mut self) {
		if let Err(e) = self.block_store.flush() {
			warn!(error = %e, "failed to flush block store");
		}
		if let Err(e) = self.chainstate.fsync_undo() {
			warn!(error = %e, "failed to fsync undo store");
		}
		self.blocks_since_flush = 0;
	}

	/// Scans in-flight requests for timeouts and resets `next_height` as needed
	///
	/// Stale entries are removed and `next_height` is set back to the minimum
	/// timed-out height so `fill_window` will re-request them
	pub fn handle_timeout_scan(&mut self) {
		let now = Instant::now();
		let mut min_stale_height: Option<u32> = None;
		let mut stale_hashes: Vec<Hash256> = Vec::new();

		for (hash, entry) in &self.in_flight {
			if now.duration_since(entry.requested_at) > REQUEST_TIMEOUT {
				stale_hashes.push(*hash);
				min_stale_height = Some(min_stale_height.map_or(entry.height, |h: u32| h.min(entry.height)));
			}
		}

		for hash in &stale_hashes {
			if let Some(entry) = self.in_flight.remove(hash) {
				warn!(
					block = %hash,
					height = entry.height,
					peer = %entry.peer,
					"block request timed out"
				);
			}
		}

		if let Some(min_height) = min_stale_height {
			debug!(
				old_next = self.next_height,
				new_next = min_height,
				stale_count = stale_hashes.len(),
				"resetting next_height after timeout scan"
			);
			self.next_height = self.next_height.min(min_height);
		}
	}

	/// Collects writers for all currently connected peers
	///
	/// Results are collected without holding `DashMap` locks across the iteration
	fn get_connected_writers(&self) -> Vec<(IpAddr, SharedTcpWriter)> {
		self.node_manager.get_connected_writers()
	}
}
