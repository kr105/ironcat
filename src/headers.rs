// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, sync::Arc};

use anyhow::{bail, Context, Result};
use tracing::{debug, error, info, warn};

use crate::{
	storage::header_store_backend::HeaderStoreBackend,
	types::{block::BlockHeader, hash::Hash256},
};

/// Maximum number of hashes in a block locator vector
const MAX_LOCATOR_HASHES: usize = 101;

/// A block header stored alongside its height in the chain
#[derive(Debug, Clone)]
pub struct StoredHeader {
	/// The block header data
	pub header: BlockHeader,
	/// Height of this header in the chain
	pub height: u32,
}

/// In-memory block header chain with dual indexing for O(1) lookup by hash or height
///
/// Headers are stored in a `HashMap` keyed by block hash for fast lookups, and a
/// parallel `Vec` maps heights to hashes for positional access. Only a single
/// linear chain is tracked (no forks)
pub struct HeaderStore {
	by_hash: HashMap<Hash256, StoredHeader>,
	by_height: Vec<Hash256>,
	backend: Option<Arc<dyn HeaderStoreBackend>>,
}

impl HeaderStore {
	/// Creates a new header store initialized with the genesis block at height 0
	///
	/// The `HashMap` is pre-allocated for 500,000 entries to reduce rehashing
	/// during initial block download. No persistence backend is used
	pub fn new(genesis_header: BlockHeader) -> Self {
		Self::with_backend(genesis_header, None)
	}

	/// Creates a header store with an optional persistence backend
	///
	/// If a backend is provided and contains stored headers, the in-memory
	/// state is populated from disk. The genesis block hash is verified on
	/// reload -- a mismatch means wrong network, so we clear the database
	/// and start fresh
	pub fn with_backend(genesis_header: BlockHeader, backend: Option<Arc<dyn HeaderStoreBackend>>) -> Self {
		let genesis_hash = genesis_header.block_hash();

		if let Some(ref be) = backend {
			match Self::try_load_from_backend(be.as_ref(), genesis_hash) {
				Ok(Some((by_hash, by_height))) => {
					#[allow(clippy::arithmetic_side_effects)] // by_height is non-empty (has genesis)
					let tip_height = by_height.len() - 1;
					info!(count = by_height.len(), tip_height, "loaded headers from disk");
					return Self {
						by_hash,
						by_height,
						backend,
					};
				}
				Ok(None) => {
					// Empty database or cleared after mismatch, persist genesis
					if let Err(e) = be.persist_header(&genesis_header, 0) {
						error!(error = %e, "failed to persist genesis header, disabling backend");
						return Self::make_fresh(genesis_header, genesis_hash, None);
					}
				}
				Err(e) => {
					error!(error = %e, "failed to load headers from backend, disabling backend");
					return Self::make_fresh(genesis_header, genesis_hash, None);
				}
			}
		}

		Self::make_fresh(genesis_header, genesis_hash, backend)
	}

	/// Attempts to load headers from the backend, validating genesis and chain continuity
	///
	/// Returns `Ok(Some(...))` on successful load, `Ok(None)` if the database
	/// was empty (or was cleared due to genesis mismatch), and `Err` on
	/// unrecoverable failure
	#[allow(clippy::type_complexity)] // internal return type, alias would obscure intent
	fn try_load_from_backend(
		be: &dyn HeaderStoreBackend,
		genesis_hash: Hash256,
	) -> Result<Option<(HashMap<Hash256, StoredHeader>, Vec<Hash256>)>> {
		let headers = be.load_all()?;

		if headers.is_empty() {
			return Ok(None);
		}

		// Verify genesis matches
		#[allow(clippy::indexing_slicing)] // headers is non-empty (checked above)
		let (ref first_header, first_height) = headers[0];
		let first_hash = first_header.block_hash();

		if first_height != 0 || first_hash != genesis_hash {
			error!("genesis mismatch in header database, clearing and starting fresh");
			be.clear()?;
			return Ok(None);
		}

		// Build in-memory state and validate chain continuity
		let mut by_hash = HashMap::with_capacity(500_000.max(headers.len()));
		let mut by_height = Vec::with_capacity(headers.len());

		let mut prev_hash = Hash256::ZERO; // genesis has prev_hash = ZERO

		for (header, height) in &headers {
			let hash = header.block_hash();

			if header.prev_hash != prev_hash {
				error!(
					height,
					expected_prev = %prev_hash,
					got_prev = %header.prev_hash,
					"chain continuity broken in header database, clearing and starting fresh"
				);
				be.clear()?;
				return Ok(None);
			}

			by_hash.insert(
				hash,
				StoredHeader {
					header: header.clone(),
					height: *height,
				},
			);
			by_height.push(hash);
			prev_hash = hash;
		}

		Ok(Some((by_hash, by_height)))
	}

	/// Creates a fresh in-memory store with only the genesis header
	fn make_fresh(
		genesis_header: BlockHeader,
		genesis_hash: Hash256,
		backend: Option<Arc<dyn HeaderStoreBackend>>,
	) -> Self {
		let mut by_hash = HashMap::with_capacity(500_000);
		by_hash.insert(
			genesis_hash,
			StoredHeader {
				header: genesis_header,
				height: 0,
			},
		);

		Self {
			by_hash,
			by_height: vec![genesis_hash],
			backend,
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

	/// Inserts a validated header into the in-memory maps
	fn commit_to_memory(&mut self, hash: Hash256, header: &BlockHeader, height: u32) {
		self.by_hash.insert(
			hash,
			StoredHeader {
				header: header.clone(),
				height,
			},
		);
		self.by_height.push(hash);
		debug!(block_hash = %hash, height, "stored header");
	}

	/// Adds a single block header to the chain, returning its height
	///
	/// The header's `prev_hash` must reference the current chain tip
	/// Duplicate headers (same block hash already stored) are rejected
	/// Headers that reference a known block that is NOT the tip are rejected
	/// (linear chain only, no forks)
	///
	/// If a backend is configured, the header is persisted BEFORE being added
	/// to in-memory state. A persist failure prevents the in-memory insert
	pub fn add_header(&mut self, header: &BlockHeader) -> Result<u32> {
		let (hash, height) = self.validate_header(header)?;

		// Persist first, then commit to memory
		if let Some(ref be) = self.backend {
			be.persist_header(header, height)
				.with_context(|| format!("failed to persist header at height {height}"))?;
		}

		self.commit_to_memory(hash, header, height);
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
		if headers.is_empty() {
			return Ok(0);
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

		// Phase 1: validate all headers, collect (hash, height) pairs
		let mut validated = Vec::new();
		// Temporarily track tip for multi-header validation without mutating state
		let mut shadow_tip = self.tip();
		for header in headers {
			let hash = header.block_hash();
			if self.by_hash.contains_key(&hash) {
				debug!(block_hash = %hash, "skipping duplicate header in batch");
				continue;
			}

			let (tip_hash, tip_height) = shadow_tip;
			let height = self.validate_chains_to_tip(hash, header, tip_hash, tip_height)?;
			validated.push((hash, header, height));
			shadow_tip = (hash, height);
		}

		if validated.is_empty() {
			return Ok(0);
		}

		// Phase 2: persist all to backend in single transaction
		if let Some(ref be) = self.backend {
			let batch: Vec<(BlockHeader, u32)> = validated
				.iter()
				.map(|(_, header, height)| ((*header).clone(), *height))
				.collect();
			be.persist_headers(&batch).context("failed to persist header batch")?;
		}

		// Phase 3: commit all to in-memory state
		#[allow(clippy::cast_possible_truncation)] // validated.len() <= headers.len() <= u32::MAX
		let added = validated.len() as u32;
		for (hash, header, height) in validated {
			self.commit_to_memory(hash, header, height);
		}

		Ok(added)
	}

	/// Looks up a stored header by its block hash
	pub fn get(&self, hash: &Hash256) -> Option<&StoredHeader> {
		self.by_hash.get(hash)
	}

	/// Returns the block hash at the given height, if it exists
	pub fn hash_at_height(&self, height: u32) -> Option<Hash256> {
		let idx = height as usize;
		self.by_height.get(idx).copied()
	}

	/// Returns the current chain tip as (hash, height)
	pub fn tip(&self) -> (Hash256, u32) {
		let height = self.height();

		// by_height always has at least the genesis entry
		#[allow(clippy::indexing_slicing)] // by_height is never empty (genesis is always present)
		let hash = self.by_height[height as usize];

		(hash, height)
	}

	/// Returns the height of the current chain tip
	pub fn height(&self) -> u32 {
		debug_assert!(
			!self.by_height.is_empty(),
			"by_height must always contain at least the genesis entry"
		);
		// Safe because by_height always contains at least the genesis entry
		#[allow(clippy::arithmetic_side_effects)] // by_height is never empty
		let h = self.by_height.len() - 1;

		#[allow(clippy::cast_possible_truncation)] // chain height will never exceed u32::MAX
		let h = h as u32;
		h
	}

	/// Returns the genesis block hash
	pub fn genesis(&self) -> Hash256 {
		// by_height always has at least the genesis entry at index 0
		#[allow(clippy::indexing_slicing)] // by_height is never empty (genesis is always present)
		let hash = self.by_height[0];
		hash
	}

	/// Builds a block locator vector from tip back to genesis
	///
	/// The first 10 hashes step back by 1, then the step doubles each iteration
	/// Genesis is always the last entry. The result is capped at `MAX_LOCATOR_HASHES`
	pub fn build_locator(&self) -> Vec<Hash256> {
		let tip_height = self.height();
		let mut locator = Vec::new();
		let mut height = tip_height;
		let mut step: u32 = 1;
		let mut consecutive = 0u32;

		loop {
			// by_height is indexed by height; height is always within bounds here
			#[allow(clippy::indexing_slicing)] // height is bounded by tip_height which is by_height.len()-1
			locator.push(self.by_height[height as usize]);

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
		#[allow(clippy::indexing_slicing)] // by_height is never empty
		let genesis = self.by_height[0];
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
			// h is bounded by tip_height which is by_height.len()-1
			#[allow(clippy::indexing_slicing)] // h <= tip_height, always a valid index
			let hash = self.by_height[h as usize];

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
}
