// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to scan for timed-out requests when blocks are in flight
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// How often to poll when idle (waiting for headers to sync)
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Flush block store to disk every N stored blocks
const FLUSH_INTERVAL: usize = 500;

/// How many heights to fetch from the header store per batch during
/// `fill_window` scanning. Larger means fewer `RwLock` acquires when
/// skipping past already-stored blocks
const SCAN_BATCH_SIZE: u32 = 4096;

/// Only refill the window when at least this many slots are free.
/// Prevents micro-batches (getdata count=1) by accumulating free slots
/// before issuing requests, producing larger batches per peer
const REFILL_THRESHOLD: usize = WINDOW_SIZE / 4;

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
	block_rx: mpsc::Receiver<(Hash256, Vec<u8>)>,
	/// Blocks currently being downloaded, keyed by block hash
	pub in_flight: HashMap<Hash256, InFlightEntry>,
	/// Next chain height to request
	pub next_height: u32,
	/// Number of in-flight requests per peer
	pub peer_counts: HashMap<IpAddr, u32>,
	/// In-memory set of all stored block hashes (avoids redb reads)
	stored_hashes: HashSet<Hash256>,
	/// Whether we were caught up on the previous loop iteration
	was_caught_up: bool,
	/// Blocks stored since last flush
	blocks_since_flush: usize,
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
	/// transactions to download
	pub fn new(
		node_manager: Arc<NodeManager>,
		block_store: Arc<BlockStore>,
		block_rx: mpsc::Receiver<(Hash256, Vec<u8>)>,
	) -> Self {
		let stored_hashes = match block_store.all_indexed_hashes() {
			Ok(hashes) => hashes,
			Err(e) => {
				warn!(error = %e, "failed to load block index hashes, will re-download stored blocks");
				HashSet::new()
			}
		};

		Self {
			node_manager,
			block_store,
			block_rx,
			in_flight: HashMap::new(),
			next_height: 1,
			peer_counts: HashMap::new(),
			stored_hashes,
			was_caught_up: false,
			blocks_since_flush: 0,
		}
	}

	/// Main download loop
	///
	/// Continuously fills the request window, drains all available blocks
	/// from the channel, and scans for timed-out requests until the
	/// channel is closed
	pub async fn run(&mut self) {
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
					self.handle_timeout_scan();
					// Flush any pending blocks on the timer tick
					if self.blocks_since_flush > 0 {
						self.flush_store();
					}
				}
			}
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
	async fn fill_window(&mut self) {
		let free_slots = WINDOW_SIZE.saturating_sub(self.in_flight.len());

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

		let mut writer_idx: usize = 0;
		let mut peer_requests: HashMap<usize, Vec<InvItem>> = HashMap::new();
		let mut new_entries: Vec<(Hash256, InFlightEntry, usize)> = Vec::new();

		// Scan heights in batches until we fill the window or reach chain tip.
		// Each batch grabs up to SCAN_BATCH_SIZE heights from the header store
		// with one lock acquire. Already-stored blocks are skipped via O(1)
		// HashSet lookup, so scanning past thousands of stored blocks is fast
		while self.in_flight.len().saturating_add(new_entries.len()) < WINDOW_SIZE && self.next_height <= chain_height {
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
				if self.in_flight.len().saturating_add(new_entries.len()) >= WINDOW_SIZE {
					break;
				}

				// Skip blocks we already have or are already in flight (O(1) in-memory)
				if self.stored_hashes.contains(hash) || self.in_flight.contains_key(hash) {
					self.next_height = self.next_height.max(height.saturating_add(1));
					made_progress = true;
					continue;
				}

				// Pick a peer under per-peer limit, round-robin
				let Some((peer_ip, _writer)) = self.pick_peer(&writers, &mut writer_idx) else {
					debug!("all peers at per-peer limit, pausing fill_window");
					// Signal outer loop to stop too
					self.next_height = self.next_height.max(height.saturating_add(1));
					return;
				};

				// Find the writer index for batching
				let widx = writers.iter().position(|(ip, _)| *ip == peer_ip).unwrap_or(0);

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

				// peer_counts bounded by WINDOW_SIZE, can't overflow u32
				#[allow(clippy::arithmetic_side_effects)]
				{
					*self.peer_counts.entry(peer_ip).or_insert(0) += 1;
				}

				self.next_height = self.next_height.max(height.saturating_add(1));
				made_progress = true;
			}

			if !made_progress {
				break;
			}
		}

		// Send batched getdata messages (one per peer)
		for (widx, items) in &peer_requests {
			// widx validated by position() above
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
				// Remove entries for this failed peer
				new_entries.retain(|(_h, _e, w)| *w != *widx);
				continue;
			}

			debug!(peer = %peer_ip, count, "sent batched getdata");
		}

		// Commit successful entries to in_flight
		for (hash, entry, _widx) in new_entries {
			self.in_flight.insert(hash, entry);
		}
	}

	/// Picks a connected peer that is below the per-peer request limit
	///
	/// Scans from `writer_idx` forward, wrapping around. Returns None if
	/// all peers are at their limit
	fn pick_peer<'a>(
		&self,
		writers: &'a [(IpAddr, SharedTcpWriter)],
		writer_idx: &mut usize,
	) -> Option<(IpAddr, &'a SharedTcpWriter)> {
		let len = writers.len();
		for _ in 0..len {
			// writer_idx modulo len is always in bounds; modulo of two usize values is side-effect free
			#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
			let (ip, ref writer) = writers[*writer_idx % len];
			*writer_idx = writer_idx.wrapping_add(1);

			let count = self.peer_counts.get(&ip).copied().unwrap_or(0);
			#[allow(clippy::cast_possible_truncation)] // PER_PEER_LIMIT fits u32
			if count < PER_PEER_LIMIT as u32 {
				return Some((ip, writer));
			}
		}
		None
	}

	/// Processes a received block: validates merkle root and stores to disk
	fn handle_received_block(&mut self, hash: Hash256, raw_bytes: &[u8]) {
		let Some(entry) = self.in_flight.remove(&hash) else {
			debug!(block = %hash, "received block not in flight, ignoring");
			return;
		};

		// Decrement peer count
		if let Some(count) = self.peer_counts.get_mut(&entry.peer) {
			*count = count.saturating_sub(1);
			if *count == 0 {
				self.peer_counts.remove(&entry.peer);
			}
		}

		// Validate merkle root
		if let Err(e) = Block::from_bytes_and_validate(raw_bytes) {
			warn!(
				block = %hash,
				height = entry.height,
				peer = %entry.peer,
				error = %e,
				"block validation failed"
			);
			return;
		}

		if let Err(e) = self.block_store.store_block_unchecked(&hash, raw_bytes) {
			warn!(
				block = %hash,
				height = entry.height,
				error = %e,
				"failed to store block"
			);
			return;
		}

		// Track in memory so fill_window skips this block without redb reads
		self.stored_hashes.insert(hash);

		self.blocks_since_flush = self.blocks_since_flush.saturating_add(1);

		// Batch flush: fsync + redb commit every FLUSH_INTERVAL blocks
		if self.blocks_since_flush >= FLUSH_INTERVAL {
			self.flush_store();
		}

		// Report actual stored count (not next_height which is the next height to request)
		#[allow(clippy::cast_possible_truncation)] // stored blocks won't exceed u32
		self.node_manager.set_block_height(self.stored_hashes.len() as u32);
		debug!(block = %hash, height = entry.height, "block stored");
	}

	/// Flushes the block store (fsync + redb commit) and resets the counter
	fn flush_store(&mut self) {
		if let Err(e) = self.block_store.flush() {
			warn!(error = %e, "failed to flush block store");
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

				if let Some(count) = self.peer_counts.get_mut(&entry.peer) {
					*count = count.saturating_sub(1);
					if *count == 0 {
						self.peer_counts.remove(&entry.peer);
					}
				}
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
