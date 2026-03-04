// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::{bail, Result};
use tracing::{debug, warn};

use crate::types::{block::BlockHeader, hash::Hash256};

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
}

impl HeaderStore {
	/// Creates a new header store initialized with the genesis block at height 0
	///
	/// The `HashMap` is pre-allocated for 500,000 entries to reduce rehashing
	/// during initial block download
	pub fn new(genesis_header: BlockHeader) -> Self {
		let genesis = genesis_header.block_hash();
		let mut by_hash = HashMap::with_capacity(500_000);
		by_hash.insert(
			genesis,
			StoredHeader {
				header: genesis_header,
				height: 0,
			},
		);

		Self {
			by_hash,
			by_height: vec![genesis],
		}
	}

	/// Adds a single block header to the chain, returning its height
	///
	/// The header's `prev_hash` must reference the current chain tip.
	/// Duplicate headers (same block hash already stored) are rejected with an error.
	/// Headers that reference a known block that is NOT the tip are rejected
	/// (linear chain only, no forks)
	pub fn add_header(&mut self, header: &BlockHeader) -> Result<u32> {
		let hash = header.block_hash();

		if self.by_hash.contains_key(&hash) {
			warn!(block_hash = %hash, "rejecting duplicate header");
			bail!("duplicate header {hash}");
		}

		let (tip_hash, tip_height) = self.tip();

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

		// Height is tip + 1; both are u32 and we won't realistically overflow
		#[allow(clippy::arithmetic_side_effects)] // height won't exceed u32::MAX in practice
		let height = tip_height + 1;

		self.by_hash.insert(
			hash,
			StoredHeader {
				header: header.clone(),
				height,
			},
		);
		self.by_height.push(hash);

		debug!(block_hash = %hash, height, "stored header");
		Ok(height)
	}

	/// Adds multiple block headers in sequence, skipping duplicates
	///
	/// Validates that headers form a continuous chain: each header's `prev_hash`
	/// must be the hash of the previous header in the batch (or connect to the
	/// current tip for the first non-duplicate). Duplicates at the start of the
	/// batch are skipped. Returns the number of headers actually added
	pub fn add_headers(&mut self, headers: &[BlockHeader]) -> Result<u32> {
		if headers.is_empty() {
			return Ok(0);
		}

		// Validate intra-batch continuity: each header must chain to the previous
		// one in the message (matching reference client behavior)
		for i in 1..headers.len() {
			// Indexing safe: i starts at 1, so i-1 >= 0 and i < len
			#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // i >= 1, so i-1 is safe
			let expected_prev = headers[i - 1].block_hash();
			#[allow(clippy::indexing_slicing)]
			if headers[i].prev_hash != expected_prev {
				warn!(
					index = i,
					expected = %expected_prev,
					got = %headers[i].prev_hash,
					"headers batch has broken chain continuity"
				);
				bail!("header at index {i} does not chain to previous header in batch");
			}
		}

		let mut added = 0u32;
		for header in headers {
			let hash = header.block_hash();
			if self.by_hash.contains_key(&hash) {
				debug!(block_hash = %hash, "skipping duplicate header in batch");
				continue;
			}

			self.add_header(header)?;
			#[allow(clippy::arithmetic_side_effects)] // bounded by slice length
			{
				added += 1;
			}
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
	pub const fn height(&self) -> u32 {
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
	/// The first 10 hashes step back by 1, then the step doubles each iteration.
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
				// step is 1 during consecutive phase; subtraction is safe because height > 0
				#[allow(clippy::arithmetic_side_effects)] // height > 0 checked above
				{
					height = height.saturating_sub(1);
				}
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
