// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;

use crate::types::block::BlockHeader;

/// Persistence backend for block headers
///
/// Implementations handle durable storage of headers so the chain survives
/// restarts. The in-memory `HeaderStore` remains the hot path for reads;
/// backends are write-through only
pub trait HeaderStoreBackend: Send + Sync {
	/// Persists a single header atomically
	fn persist_header(&self, header: &BlockHeader, height: u32) -> Result<()>;

	/// Persists a batch of headers in a single transaction
	fn persist_headers(&self, headers: &[(BlockHeader, u32)]) -> Result<()>;

	/// Loads all stored headers ordered by height (used at startup)
	fn load_all(&self) -> Result<Vec<(BlockHeader, u32)>>;

	/// Returns the number of stored headers (diagnostics)
	fn count(&self) -> Result<u64>;

	/// Removes all stored headers (used on genesis mismatch)
	fn clear(&self) -> Result<()>;
}
