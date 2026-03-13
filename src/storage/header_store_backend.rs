// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;

use crate::difficulty::U256;
use crate::types::{block::BlockHeader, hash::Hash256};

/// Persistence backend for block headers
///
/// Implementations handle durable storage of headers so the chain survives
/// restarts. The in-memory `HeaderStore` remains the hot path for reads;
/// backends are write-through only
pub trait HeaderStoreBackend: Send + Sync {
	/// Persists a single header keyed by its hash
	fn persist_header(&self, hash: &Hash256, header: &BlockHeader, height: u32, chainwork: U256) -> Result<()>;

	/// Persists a batch of headers in a single transaction
	fn persist_headers(&self, headers: &[(Hash256, BlockHeader, u32, U256)]) -> Result<()>;

	/// Loads all stored headers. Returns (hash, header, height, chainwork) tuples
	fn load_all(&self) -> Result<Vec<(Hash256, BlockHeader, u32, U256)>>;

	/// Persists an invalid tip hash
	fn persist_invalid_tip(&self, hash: &Hash256) -> Result<()>;

	/// Loads all invalid tip hashes
	fn load_invalid_tips(&self) -> Result<Vec<Hash256>>;

	/// Returns the number of stored headers (diagnostics)
	fn count(&self) -> Result<u64>;

	/// Removes all stored headers and invalid tips (used on genesis mismatch)
	fn clear(&self) -> Result<()>;
}
