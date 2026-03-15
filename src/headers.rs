// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::{debug, error, info, trace, warn};

use crate::{
	difficulty::compact::work_from_bits,
	difficulty::{self, ChainLookup, ConsensusParams, U256},
	pow,
	storage::header_store_backend::HeaderStoreBackend,
	types::{block::BlockHeader, hash::Hash256},
};

/// Maximum number of hashes in a block locator vector
const MAX_LOCATOR_HASHES: usize = 101;

/// Maximum reorg depth allowed
const MAX_REORG_DEPTH: u32 = 1_000;

/// Maximum number of non-active-chain headers to keep in memory
const MAX_FORK_HEADERS: usize = 10_000;

/// Forks older than this many blocks behind the active tip are purged
const MAX_FORK_AGE: u32 = 1_000;

/// A block header stored alongside its height and cumulative chainwork
#[derive(Debug, Clone)]
pub struct StoredHeader {
	/// The block header data
	pub header: BlockHeader,
	/// Height of this header in the chain
	pub height: u32,
	/// Cumulative proof-of-work from genesis to this header
	pub chainwork: U256,
}

/// Result of accepting a header via [`HeaderStore::accept_header`]
#[derive(Debug)]
pub enum AcceptResult {
	/// Header accepted, extends active chain or a fork with less work
	Accepted,
	/// Header accepted, fork now has more work than active chain
	NeedReorg,
	/// Header already known
	AlreadyKnown,
}

/// In-memory block header tree with dual indexing for O(1) lookup by hash or height
///
/// Headers are stored in a `HashMap` keyed by block hash. The `active_chain` Vec
/// maps heights to hashes for the currently-best chain. Fork headers are tracked
/// via the `tips` set and `best_known_tip` cache
pub struct HeaderStore {
	by_hash: HashMap<Hash256, StoredHeader>,
	active_chain: Vec<Hash256>,
	active_set: HashSet<Hash256>,
	tips: HashSet<Hash256>,
	best_known_tip: (Hash256, U256),
	invalid_tips: HashSet<Hash256>,
	backend: Option<Arc<dyn HeaderStoreBackend>>,
	params: ConsensusParams,
}

impl ChainLookup for HeaderStore {
	fn header_at(&self, height: u32) -> Option<(u32, u32)> {
		let hash = self.active_chain.get(height as usize)?;
		let stored = self.by_hash.get(hash)?;
		Some((stored.header.timestamp, stored.header.bits))
	}
}

/// Overlays pending batch headers on top of the committed store
///
/// During batch validation, headers already validated in the current batch
/// are not yet committed to the store. This struct makes them visible to
/// difficulty algorithms via [`ChainLookup`]
struct BatchLookup<'a> {
	store: &'a HeaderStore,
	/// Headers validated so far in the current batch, with sequential heights
	pending: &'a [(Hash256, &'a BlockHeader, u32, U256)],
}

impl ChainLookup for BatchLookup<'_> {
	fn header_at(&self, height: u32) -> Option<(u32, u32)> {
		// Check pending batch first (heights are sequential, so O(1) index lookup)
		if let Some(&(_, _, first_height, _)) = self.pending.first()
			&& height >= first_height
		{
			#[allow(clippy::arithmetic_side_effects)] // height >= first_height checked above
			let idx = (height - first_height) as usize;
			if let Some(&(_, header, _, _)) = self.pending.get(idx) {
				return Some((header.timestamp, header.bits));
			}
		}
		// Fall back to committed store
		self.store.header_at(height)
	}
}

/// Provides [`ChainLookup`] for fork headers not on the active chain
///
/// Walks `prev_hash` links from a fork tip back until hitting the active chain,
/// collecting fork-specific header data. Falls back to the active chain for
/// shared ancestry
struct ForkLookup<'a> {
	store: &'a HeaderStore,
	/// Fork-specific headers by height (above the fork point)
	fork_headers: HashMap<u32, (u32, u32)>,
}

impl ForkLookup<'_> {
	/// Builds a lookup by walking `prev_hash` from `start` back until hitting
	/// the active chain
	fn build(store: &HeaderStore, start: Hash256) -> ForkLookup<'_> {
		let mut fork_headers = HashMap::new();
		let mut current = start;
		while let Some(stored) = store.by_hash.get(&current) {
			if store.active_set.contains(&current) {
				break;
			}
			fork_headers.insert(stored.height, (stored.header.timestamp, stored.header.bits));
			if stored.height == 0 {
				break;
			}
			current = stored.header.prev_hash;
		}
		ForkLookup { store, fork_headers }
	}
}

impl ChainLookup for ForkLookup<'_> {
	fn header_at(&self, height: u32) -> Option<(u32, u32)> {
		if let Some(&(ts, bits)) = self.fork_headers.get(&height) {
			return Some((ts, bits));
		}
		// Fall back to active chain (shared ancestry)
		self.store.header_at(height)
	}
}

impl HeaderStore {
	/// Creates a new header store initialized with the genesis block at height 0
	///
	/// The `HashMap` is pre-allocated for 500,000 entries to reduce rehashing
	/// during initial block download. No persistence backend is used
	pub fn new(genesis_header: BlockHeader, params: ConsensusParams) -> Self {
		Self::with_backend(genesis_header, params, None)
	}

	/// Creates a header store with an optional persistence backend
	///
	/// If a backend is provided and contains stored headers, the in-memory
	/// state is populated from disk. The genesis block hash is verified on
	/// reload -- a mismatch means wrong network, so we clear the database
	/// and start fresh
	pub fn with_backend(
		genesis_header: BlockHeader,
		params: ConsensusParams,
		backend: Option<Arc<dyn HeaderStoreBackend>>,
	) -> Self {
		let genesis_hash = genesis_header.block_hash();

		if let Some(ref be) = backend {
			match Self::try_load_from_backend(be.as_ref(), genesis_hash) {
				Ok(Some((by_hash, active_chain, active_set, tips, best_known_tip, invalid_tips))) => {
					#[allow(clippy::arithmetic_side_effects)] // active_chain is non-empty (has genesis)
					let tip_height = active_chain.len() - 1;
					info!(count = by_hash.len(), tip_height, "loaded headers from disk");
					return Self {
						by_hash,
						active_chain,
						active_set,
						tips,
						best_known_tip,
						invalid_tips,
						backend,
						params,
					};
				}
				Ok(None) => {
					// Empty database or cleared after mismatch, persist genesis
					let genesis_chainwork = work_from_bits(genesis_header.bits);
					if let Err(e) = be.persist_header(&genesis_hash, &genesis_header, 0, genesis_chainwork) {
						error!(error = %e, "failed to persist genesis header, disabling backend");
						return Self::make_fresh(genesis_header, genesis_hash, params, None);
					}
				}
				Err(e) => {
					error!(error = %e, "failed to load headers from backend, disabling backend");
					return Self::make_fresh(genesis_header, genesis_hash, params, None);
				}
			}
		}

		Self::make_fresh(genesis_header, genesis_hash, params, backend)
	}

	/// Attempts to load headers from the backend, validating genesis and chain continuity
	///
	/// Returns `Ok(Some(...))` on successful load, `Ok(None)` if the database
	/// was empty (or was cleared due to genesis mismatch), and `Err` on
	/// unrecoverable failure
	#[allow(clippy::type_complexity)] // internal return type, alias would obscure intent
	pub(crate) fn try_load_from_backend(
		be: &dyn HeaderStoreBackend,
		genesis_hash: Hash256,
	) -> Result<
		Option<(
			HashMap<Hash256, StoredHeader>,
			Vec<Hash256>,
			HashSet<Hash256>,
			HashSet<Hash256>,
			(Hash256, U256),
			HashSet<Hash256>,
		)>,
	> {
		// Load invalid tips
		let invalid_tips: HashSet<Hash256> = be.load_invalid_tips()?.into_iter().collect();

		let raw = be.load_all()?;

		if raw.is_empty() {
			return Ok(None);
		}

		// Build by_hash from loaded data
		let mut by_hash = HashMap::with_capacity(500_000.max(raw.len()));
		for (hash, header, height, chainwork) in &raw {
			by_hash.insert(
				*hash,
				StoredHeader {
					header: header.clone(),
					height: *height,
					chainwork: *chainwork,
				},
			);
		}

		// Verify genesis exists
		if !by_hash.contains_key(&genesis_hash) {
			error!("genesis mismatch in header database, clearing and starting fresh");
			be.clear()?;
			return Ok(None);
		}

		// Identify tips (headers not referenced as prev_hash by anyone)
		let referenced: HashSet<Hash256> = by_hash.values().map(|s| s.header.prev_hash).collect();
		let tips: HashSet<Hash256> = by_hash
			.keys()
			.filter(|h| !referenced.contains(h) && !invalid_tips.contains(h))
			.copied()
			.collect();

		// Find best tip by chainwork
		let best_known_tip = tips
			.iter()
			.filter_map(|t| by_hash.get(t).map(|s| (*t, s.chainwork)))
			.max_by_key(|(_, cw)| *cw)
			.unwrap_or((genesis_hash, U256::zero()));

		// Build active_chain by walking from best tip to genesis
		let mut active_chain = Vec::new();
		let mut current = best_known_tip.0;
		loop {
			active_chain.push(current);
			let stored = by_hash.get(&current).context("broken chain during load")?;
			if stored.height == 0 {
				break;
			}
			current = stored.header.prev_hash;
		}
		active_chain.reverse();

		// Verify heights are sequential in the active chain
		for (i, hash) in active_chain.iter().enumerate() {
			#[allow(clippy::indexing_slicing)] // hash came from active_chain which came from by_hash
			let stored = &by_hash[hash];
			#[allow(clippy::cast_possible_truncation)] // i bounded by chain length
			if stored.height != i as u32 {
				error!(
					expected = i,
					got = stored.height,
					"active chain height mismatch, clearing and starting fresh"
				);
				be.clear()?;
				return Ok(None);
			}
		}

		let active_set: HashSet<Hash256> = active_chain.iter().copied().collect();

		Ok(Some((
			by_hash,
			active_chain,
			active_set,
			tips,
			best_known_tip,
			invalid_tips,
		)))
	}

	/// Creates a fresh in-memory store with only the genesis header
	fn make_fresh(
		genesis_header: BlockHeader,
		genesis_hash: Hash256,
		params: ConsensusParams,
		backend: Option<Arc<dyn HeaderStoreBackend>>,
	) -> Self {
		let genesis_chainwork = work_from_bits(genesis_header.bits);
		let mut by_hash = HashMap::with_capacity(500_000);
		by_hash.insert(
			genesis_hash,
			StoredHeader {
				header: genesis_header,
				height: 0,
				chainwork: genesis_chainwork,
			},
		);

		let mut active_set = HashSet::new();
		active_set.insert(genesis_hash);

		let mut tips = HashSet::new();
		tips.insert(genesis_hash);

		Self {
			by_hash,
			active_chain: vec![genesis_hash],
			active_set,
			tips,
			best_known_tip: (genesis_hash, genesis_chainwork),
			invalid_tips: HashSet::new(),
			backend,
			params,
		}
	}

	/// Applies pre-loaded backend data and stores the backend reference
	///
	/// Called after [`try_load_from_backend`] has done the heavy lifting
	/// outside of any lock. This method only does a fast pointer swap
	#[allow(clippy::type_complexity)] // matches try_load_from_backend return type
	pub(crate) fn apply_backend_load(
		&mut self,
		result: Result<
			Option<(
				HashMap<Hash256, StoredHeader>,
				Vec<Hash256>,
				HashSet<Hash256>,
				HashSet<Hash256>,
				(Hash256, U256),
				HashSet<Hash256>,
			)>,
		>,
		backend: Arc<dyn HeaderStoreBackend>,
	) {
		match result {
			Ok(Some((by_hash, active_chain, active_set, tips, best_known_tip, invalid_tips))) => {
				#[allow(clippy::arithmetic_side_effects)] // active_chain is non-empty (has genesis)
				let tip_height = active_chain.len() - 1;
				info!(count = by_hash.len(), tip_height, "loaded headers from disk");
				self.by_hash = by_hash;
				self.active_chain = active_chain;
				self.active_set = active_set;
				self.tips = tips;
				self.best_known_tip = best_known_tip;
				self.invalid_tips = invalid_tips;
				self.backend = Some(backend);
			}
			Ok(None) => {
				// Empty database or cleared after genesis mismatch, persist our genesis
				let genesis_hash = self.genesis();
				#[allow(clippy::indexing_slicing)] // genesis is always present
				let genesis_header = &self.by_hash[&genesis_hash].header;
				let genesis_chainwork = work_from_bits(genesis_header.bits);
				if let Err(e) = backend.persist_header(&genesis_hash, genesis_header, 0, genesis_chainwork) {
					error!(error = %e, "failed to persist genesis header, disabling backend");
					return;
				}
				self.backend = Some(backend);
			}
			Err(e) => {
				error!(error = %e, "failed to load headers from backend, disabling backend");
			}
		}
	}

	/// Validates a header chains to the given tip, returning the new height
	///
	/// Shared logic between `add_header` (real tip) and `add_headers` (shadow tip)
	fn validate_chains_to_tip(
		&self,
		hash: Hash256,
		header: &BlockHeader,
		tip_hash: Hash256,
		tip_height: u32,
	) -> Result<u32> {
		if header.prev_hash != tip_hash {
			if self.by_hash.contains_key(&header.prev_hash) {
				warn!(
					block_hash = %hash,
					prev_hash = %header.prev_hash,
					tip = %tip_hash,
					"rejecting header that forks off a non-tip block"
				);
				bail!("header {hash} chains to {}, not tip {tip_hash}", header.prev_hash);
			}
			warn!(
				block_hash = %hash,
				prev_hash = %header.prev_hash,
				"rejecting orphan header with unknown prev_hash"
			);
			bail!("orphan header {hash}: unknown prev_hash {}", header.prev_hash);
		}

		let height = tip_height.checked_add(1).context("chain height would overflow u32")?;

		Ok(height)
	}

	/// Validates a single header against the current tip without modifying state
	///
	/// Returns `(hash, height)` on success
	fn validate_header(&self, header: &BlockHeader) -> Result<(Hash256, u32)> {
		let hash = header.block_hash();

		if self.by_hash.contains_key(&hash) {
			warn!(block_hash = %hash, "rejecting duplicate header");
			bail!("duplicate header {hash}");
		}

		let (tip_hash, tip_height) = self.tip();
		let height = self.validate_chains_to_tip(hash, header, tip_hash, tip_height)?;

		Ok((hash, height))
	}

	/// Validates proof-of-work and checkpoint hash for a header
	///
	/// `PoW` is skipped for heights at or below the last checkpoint (assumed valid).
	/// At exact checkpoint heights, the block hash must match the checkpoint value
	fn validate_pow_and_checkpoint(&self, hash: Hash256, header: &BlockHeader, height: u32) -> Result<()> {
		let last_cp = self.params.last_checkpoint_height();

		// At exact checkpoint heights, verify the hash matches
		if let Some(expected) = self.params.checkpoint_hash_at(height) {
			let expected_hash = Hash256::from_bytes(expected);
			if hash != expected_hash {
				warn!(
					height,
					expected = %expected_hash,
					got = %hash,
					"block hash does not match checkpoint"
				);
				bail!("checkpoint mismatch at height {height}: expected {expected_hash}, got {hash}");
			}
		}

		// PoW validation: skip for heights at or below last checkpoint
		if height > last_cp && !pow::check_proof_of_work(header) {
			warn!(height, block_hash = %hash, "header fails proof-of-work validation");
			bail!("header at height {height} fails proof-of-work check");
		}

		Ok(())
	}

	/// Inserts a validated header into the in-memory maps (active chain extension only)
	fn commit_to_memory(&mut self, hash: Hash256, header: &BlockHeader, height: u32, chainwork: U256) {
		// Update tips: the new header's parent is no longer a tip since it has a child
		self.tips.remove(&header.prev_hash);
		self.tips.insert(hash);

		self.by_hash.insert(
			hash,
			StoredHeader {
				header: header.clone(),
				height,
				chainwork,
			},
		);
		self.active_chain.push(hash);
		self.active_set.insert(hash);

		if chainwork > self.best_known_tip.1 {
			self.best_known_tip = (hash, chainwork);
		}
		debug!(block_hash = %hash, height, "stored header");
	}

	/// Adds a single block header to the chain, returning its height
	///
	/// The header's `prev_hash` must reference the current chain tip.
	/// Duplicate headers (same block hash already stored) are rejected.
	/// Headers that reference a known block that is NOT the tip are rejected
	/// (linear chain only during IBD, no forks).
	///
	/// If a backend is configured, the header is persisted BEFORE being added
	/// to in-memory state. A persist failure prevents the in-memory insert
	pub fn add_header(&mut self, header: &BlockHeader) -> Result<u32> {
		let (hash, height) = self.validate_header(header)?;

		// Validate difficulty target matches expected value
		let expected_bits = difficulty::get_next_work_required(height, self as &Self, &self.params)?;
		if header.bits != expected_bits {
			warn!(
				height,
				expected = format_args!("{expected_bits:#010x}"),
				got = format_args!("{:#010x}", header.bits),
				"rejecting header with invalid difficulty"
			);
			bail!(
				"invalid difficulty at height {height}: expected {expected_bits:#010x}, got {:#010x}",
				header.bits
			);
		}

		self.validate_pow_and_checkpoint(hash, header, height)?;

		// Compute chainwork
		let parent_chainwork = self
			.active_chain
			.last()
			.and_then(|h| self.by_hash.get(h))
			.map_or_else(U256::zero, |s| s.chainwork);
		#[allow(clippy::arithmetic_side_effects)] // chainwork addition cannot overflow U256 in practice
		let chainwork = parent_chainwork + work_from_bits(header.bits);

		// Persist first, then commit to memory
		if let Some(ref be) = self.backend {
			be.persist_header(&hash, header, height, chainwork)
				.with_context(|| format!("failed to persist header at height {height}"))?;
		}

		self.commit_to_memory(hash, header, height, chainwork);
		Ok(height)
	}

	/// Adds multiple block headers in sequence, skipping duplicates
	///
	/// Validates that headers form a continuous chain: each header's `prev_hash`
	/// must be the hash of the previous header in the batch (or connect to the
	/// current tip for the first non-duplicate). Duplicates at the start of the
	/// batch are skipped. Returns the number of headers actually added
	///
	/// If a backend is configured, the entire batch is persisted in a single
	/// transaction BEFORE being added to in-memory state
	pub fn add_headers(&mut self, headers: &[BlockHeader]) -> Result<u32> {
		let validated = self.validate_batch(headers)?;

		if validated.is_empty() {
			return Ok(0);
		}

		// Persist first, then commit to memory (safe API: no side effects on failure)
		if let Some(ref be) = self.backend {
			let batch: Vec<(Hash256, BlockHeader, u32, U256)> = validated
				.iter()
				.map(|&(hash, header, height, chainwork)| (hash, header.clone(), height, chainwork))
				.collect();
			be.persist_headers(&batch).context("failed to persist header batch")?;
		}

		#[allow(clippy::cast_possible_truncation)] // validated.len() <= headers.len() <= u32::MAX
		let added = validated.len() as u32;
		for (hash, header, height, chainwork) in validated {
			self.commit_to_memory(hash, header, height, chainwork);
		}

		Ok(added)
	}

	/// Validates and commits headers to in-memory state, returning the batch
	/// for external persistence
	///
	/// Unlike `add_headers`, this commits to memory immediately without persisting.
	/// The caller is responsible for calling `persist_batch` afterward. This
	/// allows releasing locks between commit and persist for better concurrency
	pub fn validate_and_commit(&mut self, headers: &[BlockHeader]) -> Result<Vec<(Hash256, BlockHeader, u32, U256)>> {
		let validated = self.validate_batch(headers)?;

		if validated.is_empty() {
			return Ok(Vec::new());
		}

		let batch: Vec<(Hash256, BlockHeader, u32, U256)> = validated
			.iter()
			.map(|&(hash, header, height, chainwork)| (hash, header.clone(), height, chainwork))
			.collect();
		for (hash, header, height, chainwork) in validated {
			self.commit_to_memory(hash, header, height, chainwork);
		}

		Ok(batch)
	}

	/// Validates a batch of headers without modifying state
	///
	/// Two-phase validation: phase 1 checks chain continuity, difficulty, and
	/// checkpoint hashes sequentially. Phase 2 validates scrypt proof-of-work
	/// in parallel using rayon, since each scrypt call is independent and
	/// CPU-expensive (~2ms per header)
	fn validate_batch<'a>(&self, headers: &'a [BlockHeader]) -> Result<Vec<(Hash256, &'a BlockHeader, u32, U256)>> {
		if headers.is_empty() {
			return Ok(Vec::new());
		}

		// Validate intra-batch continuity: each header must chain to the previous
		// one in the message (matching reference client behavior)
		for i in 1..headers.len() {
			#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
			// i >= 1 so i-1 is safe; i < len by loop bound
			let expected_prev = headers[i - 1].block_hash();
			#[allow(clippy::indexing_slicing)] // i < headers.len() by loop bound
			let current = &headers[i];
			if current.prev_hash != expected_prev {
				warn!(
					index = i,
					expected = %expected_prev,
					got = %current.prev_hash,
					"headers batch has broken chain continuity"
				);
				bail!("header at index {i} does not chain to previous header in batch");
			}
		}

		// Phase 1: Sequential validation (chain linkage, difficulty, checkpoints)
		let last_cp = self.params.last_checkpoint_height();
		let mut validated: Vec<(Hash256, &BlockHeader, u32, U256)> = Vec::new();
		let mut shadow_tip = self.tip();
		for header in headers {
			let hash = header.block_hash();
			if self.by_hash.contains_key(&hash) {
				trace!(block_hash = %hash, "skipping duplicate header in batch");
				continue;
			}

			let (tip_hash, tip_height) = shadow_tip;
			let height = self.validate_chains_to_tip(hash, header, tip_hash, tip_height)?;

			// Validate difficulty using overlay that sees both store and pending batch
			let lookup = BatchLookup {
				store: self,
				pending: &validated,
			};
			let expected_bits = difficulty::get_next_work_required(height, &lookup, &self.params)?;
			if header.bits != expected_bits {
				warn!(
					height,
					expected = format_args!("{expected_bits:#010x}"),
					got = format_args!("{:#010x}", header.bits),
					"rejecting header with invalid difficulty"
				);
				bail!(
					"invalid difficulty at height {height}: expected {expected_bits:#010x}, got {:#010x}",
					header.bits
				);
			}

			// Checkpoint hash verification (fast, stays in sequential phase)
			if let Some(expected) = self.params.checkpoint_hash_at(height) {
				let expected_hash = Hash256::from_bytes(expected);
				if hash != expected_hash {
					warn!(
						height,
						expected = %expected_hash,
						got = %hash,
						"block hash does not match checkpoint"
					);
					bail!("checkpoint mismatch at height {height}: expected {expected_hash}, got {hash}");
				}
			}

			// Compute cumulative chainwork
			let prev_hash = header.prev_hash;
			let parent_chainwork = self.by_hash.get(&prev_hash).map_or_else(
				|| {
					// Parent is in the pending batch (not yet committed)
					validated
						.iter()
						.rev()
						.find(|(h, _, _, _)| *h == prev_hash)
						.map_or_else(U256::zero, |(_, _, _, cw)| *cw)
				},
				|stored| stored.chainwork,
			);
			#[allow(clippy::arithmetic_side_effects)] // chainwork addition cannot overflow U256 in practice
			let chainwork = parent_chainwork + work_from_bits(header.bits);

			validated.push((hash, header, height, chainwork));
			shadow_tip = (hash, height);
		}

		// Phase 2: Parallel PoW validation (scrypt, expensive)
		// Only run for headers above the last checkpoint height
		let needs_pow = validated.iter().any(|&(_, _, h, _)| h > last_cp);
		if needs_pow {
			use rayon::prelude::*;

			let pow_failure = validated
				.par_iter()
				.filter(|(_, _, height, _)| *height > last_cp)
				.find_any(|(_, header, _, _)| !pow::check_proof_of_work(header));

			if let Some(&(hash, _, height, _)) = pow_failure {
				warn!(height, block_hash = %hash, "header fails proof-of-work validation");
				bail!("header at height {height} fails proof-of-work check");
			}
		}

		Ok(validated)
	}

	/// Persists a batch of headers to the backend (if configured)
	///
	/// This only requires `&self` so it can be called with a read lock,
	/// allowing other readers to proceed concurrently
	pub fn persist_batch(&self, batch: &[(Hash256, BlockHeader, u32, U256)]) -> Result<()> {
		if let Some(ref be) = self.backend {
			be.persist_headers(batch).context("failed to persist header batch")?;
		}
		Ok(())
	}

	/// Looks up a stored header by its block hash
	pub fn get(&self, hash: &Hash256) -> Option<&StoredHeader> {
		self.by_hash.get(hash)
	}

	/// Returns the block hash at the given height on the active chain, if it exists
	pub fn hash_at_height(&self, height: u32) -> Option<Hash256> {
		let idx = height as usize;
		self.active_chain.get(idx).copied()
	}

	/// Returns block hashes for a range of heights in a single call
	///
	/// Avoids per-height `RwLock` acquisition by extracting a batch of
	/// (height, hash) pairs while holding the lock once. Heights beyond
	/// the current tip are silently skipped
	pub fn hashes_in_range(&self, start: u32, end: u32) -> Vec<(u32, Hash256)> {
		let start_idx = start as usize;
		let end_idx = (end as usize).min(self.active_chain.len());
		if start_idx >= end_idx {
			return Vec::new();
		}
		// start_idx..end_idx is bounded by active_chain.len() via the min() above
		#[allow(clippy::indexing_slicing)]
		let slice = &self.active_chain[start_idx..end_idx];
		slice
			.iter()
			.enumerate()
			.map(|(i, &hash)| {
				// i bounded by slice len which fits in u32 (chain heights are u32)
				#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
				let height = start + i as u32;
				(height, hash)
			})
			.collect()
	}

	/// Returns the compact target (nBits) of the current chain tip
	pub fn tip_bits(&self) -> u32 {
		let (tip_hash, _) = self.tip();
		// tip hash always exists in by_hash
		#[allow(clippy::indexing_slicing)] // tip hash is always present
		self.by_hash[&tip_hash].header.bits
	}

	/// Returns the current active chain tip as (hash, height)
	pub fn tip(&self) -> (Hash256, u32) {
		let height = self.height();

		// active_chain always has at least the genesis entry
		#[allow(clippy::indexing_slicing)] // active_chain is never empty (genesis is always present)
		let hash = self.active_chain[height as usize];

		(hash, height)
	}

	/// Returns the height of the current active chain tip
	pub fn height(&self) -> u32 {
		debug_assert!(
			!self.active_chain.is_empty(),
			"active_chain must always contain at least the genesis entry"
		);
		// Safe because active_chain always contains at least the genesis entry
		#[allow(clippy::arithmetic_side_effects)] // active_chain is never empty
		let h = self.active_chain.len() - 1;

		#[allow(clippy::cast_possible_truncation)] // chain height will never exceed u32::MAX
		let h = h as u32;
		h
	}

	/// Returns the genesis block hash
	pub fn genesis(&self) -> Hash256 {
		// active_chain always has at least the genesis entry at index 0
		#[allow(clippy::indexing_slicing)] // active_chain is never empty (genesis is always present)
		let hash = self.active_chain[0];
		hash
	}

	/// Builds a block locator vector from tip back to genesis
	///
	/// The first 10 hashes step back by 1, then the step doubles each iteration.
	/// Genesis is always the last entry. The result is capped at `MAX_LOCATOR_HASHES`
	pub fn build_locator(&self) -> Vec<Hash256> {
		let tip_height = self.height();
		let mut locator = Vec::new();
		let mut height = tip_height;
		let mut step: u32 = 1;
		let mut consecutive = 0u32;

		loop {
			// active_chain is indexed by height; height is always within bounds here
			#[allow(clippy::indexing_slicing)] // height is bounded by tip_height which is active_chain.len()-1
			locator.push(self.active_chain[height as usize]);

			if locator.len() >= MAX_LOCATOR_HASHES {
				break;
			}

			if height == 0 {
				break;
			}

			if consecutive < 10 {
				#[allow(clippy::arithmetic_side_effects)] // consecutive < 10, so +1 stays well within u32
				{
					consecutive += 1;
				}
				height = height.saturating_sub(1);
			} else {
				// Once we pass the first 10, start doubling step and ensure we don't go below 0
				height = height.saturating_sub(step);
				// Double step, cap at u32::MAX to avoid overflow
				step = step.saturating_mul(2);
			}
		}

		// Always include genesis if not already present
		#[allow(clippy::indexing_slicing)] // active_chain is never empty
		let genesis = self.active_chain[0];
		if locator.last().copied() != Some(genesis) && locator.len() < MAX_LOCATOR_HASHES {
			locator.push(genesis);
		}

		locator
	}

	/// Returns up to `max_count` headers starting after `start_height`
	///
	/// Iterates the height index from `start_height + 1` upward. Stops early if
	/// `stop_hash` is non-zero and a header with that hash is encountered (that
	/// header is included in the result). Returns an empty vec if `start_height`
	/// is already at or beyond the tip
	pub fn get_headers_after(&self, start_height: u32, max_count: usize, stop_hash: Hash256) -> Vec<BlockHeader> {
		let tip_height = self.height();
		if start_height >= tip_height {
			return Vec::new();
		}

		let mut headers = Vec::new();
		// start_height + 1 is safe: start_height < tip_height <= u32::MAX
		#[allow(clippy::arithmetic_side_effects)] // start_height < tip_height, so +1 won't overflow
		let mut h = start_height + 1;

		while h <= tip_height && headers.len() < max_count {
			// h is bounded by tip_height which is active_chain.len()-1
			#[allow(clippy::indexing_slicing)] // h <= tip_height, always a valid index
			let hash = self.active_chain[h as usize];

			if let Some(stored) = self.by_hash.get(&hash) {
				headers.push(stored.header.clone());
				if stop_hash != Hash256::ZERO && hash == stop_hash {
					break;
				}
			}

			// h <= tip_height <= u32::MAX, and we break before overflow
			#[allow(clippy::arithmetic_side_effects)] // h <= tip_height < u32::MAX
			{
				h += 1;
			}
		}

		headers
	}

	/// Returns a reference to the consensus params
	pub const fn params(&self) -> &ConsensusParams {
		&self.params
	}

	/// Accepts a single header that may create or extend a fork
	///
	/// Unlike `add_header` which only accepts headers extending the active tip,
	/// this method accepts headers extending any known block. Used post-IBD
	pub fn accept_header(&mut self, header: BlockHeader) -> Result<AcceptResult> {
		let hash = header.block_hash();

		// Already known?
		if self.by_hash.contains_key(&hash) {
			return Ok(AcceptResult::AlreadyKnown);
		}

		// Reject headers extending invalid tips
		if self.invalid_tips.contains(&header.prev_hash) {
			bail!("header extends invalid chain");
		}

		// Get parent
		let parent = self
			.by_hash
			.get(&header.prev_hash)
			.context("orphan header: unknown prev_hash")?;
		let height = parent.height.checked_add(1).context("chain height overflow")?;
		#[allow(clippy::arithmetic_side_effects)] // chainwork addition cannot overflow U256 in practice
		let chainwork = parent.chainwork + work_from_bits(header.bits);

		// Difficulty check using ForkLookup for correct ancestor traversal
		let fork_lookup = ForkLookup::build(self, header.prev_hash);
		let expected_bits = difficulty::get_next_work_required(height, &fork_lookup, &self.params)?;
		if header.bits != expected_bits {
			bail!("invalid difficulty on fork header at height {height}");
		}

		// Validate PoW (skip if below checkpoint)
		let last_cp = self.params.last_checkpoint_height();
		if height > last_cp && !pow::check_proof_of_work(&header) {
			bail!("fork header at height {height} fails proof-of-work check");
		}

		// Persist before moving header into StoredHeader
		if let Some(ref be) = self.backend {
			be.persist_header(&hash, &header, height, chainwork)?;
		}

		// Update tips: remove parent from tips (no longer a tip), add new hash
		let prev_hash = header.prev_hash;
		self.tips.remove(&prev_hash);
		self.tips.insert(hash);

		// Add to tree
		let stored = StoredHeader {
			header,
			height,
			chainwork,
		};
		self.by_hash.insert(hash, stored);

		// Check if this creates a better chain
		if chainwork > self.best_known_tip.1 {
			self.best_known_tip = (hash, chainwork);
			// Is this NOT extending the active chain?
			let active_tip = self.active_chain.last();
			if active_tip != Some(&prev_hash) {
				return Ok(AcceptResult::NeedReorg);
			}
			// Extends active chain directly -- just append
			self.active_chain.push(hash);
			self.active_set.insert(hash);
		}

		Ok(AcceptResult::Accepted)
	}

	/// Finds the common ancestor of two blocks in the header tree
	pub fn find_fork_point(&self, hash_a: Hash256, hash_b: Hash256) -> Result<Hash256> {
		let mut a = hash_a;
		let mut b = hash_b;
		let mut height_a = self.by_hash.get(&a).context("hash_a not found")?.height;
		let mut height_b = self.by_hash.get(&b).context("hash_b not found")?.height;

		// Equalize heights
		while height_a > height_b {
			let stored = self.by_hash.get(&a).context("broken chain")?;
			a = stored.header.prev_hash;
			#[allow(clippy::arithmetic_side_effects)] // height_a > height_b, so decrement is safe
			{
				height_a -= 1;
			}
		}
		while height_b > height_a {
			let stored = self.by_hash.get(&b).context("broken chain")?;
			b = stored.header.prev_hash;
			#[allow(clippy::arithmetic_side_effects)] // height_b > height_a, so decrement is safe
			{
				height_b -= 1;
			}
		}

		// Walk both back until they meet (bounded by equalized height)
		let mut steps: u32 = 0;
		while a != b {
			if steps > height_a {
				bail!("find_fork_point exceeded depth {steps} without convergence (corrupt chain data?)");
			}
			let sa = self.by_hash.get(&a).context("broken chain")?;
			let sb = self.by_hash.get(&b).context("broken chain")?;
			a = sa.header.prev_hash;
			b = sb.header.prev_hash;
			steps = steps.saturating_add(1);
		}

		Ok(a)
	}

	/// Returns (hash, height) pairs from `from` (exclusive) to `to` (inclusive)
	/// in ascending height order
	pub fn get_path(&self, from: Hash256, to: Hash256) -> Result<Vec<(Hash256, u32)>> {
		let mut path = Vec::new();
		let mut current = to;
		while current != from {
			let stored = self.by_hash.get(&current).context("broken chain in get_path")?;
			path.push((current, stored.height));
			current = stored.header.prev_hash;
		}
		path.reverse();
		Ok(path)
	}

	/// Switches the active chain to a new tip by rebuilding `active_chain` and `active_set`
	pub fn switch_active_chain(&mut self, new_tip: Hash256) -> Result<()> {
		let old_tip = *self.active_chain.last().context("empty active chain")?;

		// Rebuild active_chain by walking back from new_tip
		let mut chain = Vec::new();
		let mut current = new_tip;
		loop {
			chain.push(current);
			let stored = self.by_hash.get(&current).context("broken chain")?;
			if stored.height == 0 {
				break; // genesis
			}
			current = stored.header.prev_hash;
		}
		chain.reverse();
		self.active_set = chain.iter().copied().collect();
		self.active_chain = chain;

		// Update tips: old active tip becomes fork tip, new tip is now active
		self.tips.insert(old_tip);
		self.tips.remove(&new_tip);

		self.recompute_best_tip();

		Ok(())
	}

	/// Marks a tip as invalid, removing it and descendants from the valid tips set
	pub fn mark_invalid(&mut self, hash: Hash256) {
		self.invalid_tips.insert(hash);
		self.tips.remove(&hash);

		// Also remove any tips that descend from this hash
		let descendants: Vec<Hash256> = self
			.tips
			.iter()
			.filter(|tip| self.descends_from(**tip, hash))
			.copied()
			.collect();
		for desc in &descendants {
			self.tips.remove(desc);
			self.invalid_tips.insert(*desc);
		}

		// Persist original and all descendant invalid tips
		if let Some(ref be) = self.backend {
			if let Err(e) = be.persist_invalid_tip(&hash) {
				error!(error = %e, hash = %hash, "failed to persist invalid tip");
			}
			for desc in &descendants {
				if let Err(e) = be.persist_invalid_tip(desc) {
					error!(error = %e, hash = %desc, "failed to persist invalid tip descendant");
				}
			}
		}

		// Recompute best_known_tip
		self.recompute_best_tip();
	}

	/// Returns the best known tip as (hash, height, chainwork)
	pub fn best_tip(&self) -> (Hash256, u32, U256) {
		let (hash, chainwork) = self.best_known_tip;
		let height = self.by_hash.get(&hash).map_or(0, |s| s.height);
		(hash, height, chainwork)
	}

	/// Returns the maximum reorg depth allowed
	pub const fn max_reorg_depth() -> u32 {
		MAX_REORG_DEPTH
	}

	/// Purges fork headers that are too old or too numerous
	pub fn purge_stale_forks(&mut self, active_tip_height: u32) {
		// Age-based: remove tips more than MAX_FORK_AGE behind
		let stale: Vec<Hash256> = self
			.tips
			.iter()
			.filter(|tip| {
				self.by_hash
					.get(tip)
					.is_some_and(|s| active_tip_height.saturating_sub(s.height) > MAX_FORK_AGE)
			})
			.copied()
			.collect();
		for tip in &stale {
			self.remove_fork(*tip);
		}

		// Count-based: if too many non-active headers, purge oldest
		let non_active_count = self.by_hash.len().saturating_sub(self.active_chain.len());
		if non_active_count > MAX_FORK_HEADERS {
			let mut fork_tips: Vec<(Hash256, u32)> = self
				.tips
				.iter()
				.filter_map(|tip| self.by_hash.get(tip).map(|s| (*tip, s.height)))
				.collect();
			fork_tips.sort_unstable_by_key(|(_, h)| *h);
			for (tip, _) in fork_tips {
				if self.by_hash.len().saturating_sub(self.active_chain.len()) <= MAX_FORK_HEADERS {
					break;
				}
				self.remove_fork(tip);
			}
		}

		self.recompute_best_tip();
	}

	/// Checks if `tip` descends from `ancestor` by walking `prev_hash` links
	fn descends_from(&self, tip: Hash256, ancestor: Hash256) -> bool {
		let mut current = tip;
		while current != ancestor {
			match self.by_hash.get(&current) {
				Some(stored) if stored.height > 0 => current = stored.header.prev_hash,
				_ => return false,
			}
		}
		true
	}

	/// Recomputes `best_known_tip` from the active chain tip and all valid tips
	fn recompute_best_tip(&mut self) {
		// Active chain tip is always a candidate
		let active_tip = self.active_chain.last().copied().unwrap_or(Hash256::ZERO);
		let active_work = self.by_hash.get(&active_tip).map_or_else(U256::zero, |s| s.chainwork);
		let mut best = (active_tip, active_work);

		for &tip in &self.tips {
			if let Some(stored) = self.by_hash.get(&tip)
				&& stored.chainwork > best.1
			{
				best = (tip, stored.chainwork);
			}
		}
		self.best_known_tip = best;
	}

	/// Removes a fork by walking from its tip back to the active chain,
	/// deleting headers that are not shared with other forks
	fn remove_fork(&mut self, tip: Hash256) {
		self.tips.remove(&tip);
		let mut current = tip;
		loop {
			// Don't remove active chain members
			if self.active_set.contains(&current) {
				break;
			}
			let prev = match self.by_hash.get(&current) {
				Some(s) => s.header.prev_hash,
				None => break,
			};
			self.by_hash.remove(&current);
			// Stop if prev still has other children in the tree (shared ancestor)
			let has_remaining_child = self.by_hash.values().any(|s| s.header.prev_hash == prev);
			if has_remaining_child {
				break;
			}
			current = prev;
		}
	}
}
