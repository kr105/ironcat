// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{bail, Context, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use tracing::error;

use crate::types::block::{BlockHeader, HEADER_SIZE};

use super::header_store_backend::HeaderStoreBackend;

/// Table mapping height to the raw 80-byte serialized header
const HEADERS: TableDefinition<u32, &[u8; HEADER_SIZE]> = TableDefinition::new("headers");

/// Sanity cap for header count to prevent memory exhaustion from corrupted databases
const MAX_LOADABLE_HEADERS: u64 = 10_000_000;

/// redb-backed persistent storage for block headers
///
/// Uses a single table keyed by height. Block hashes are recomputed from
/// header bytes during `load_all` -- no redundant index to get out of sync
pub struct RedbHeaderStore {
	db: Database,
}

impl RedbHeaderStore {
	/// Opens (or creates) a header database at the given path
	///
	/// Creates the table on first open
	pub fn open(path: &Path) -> Result<Self> {
		let db = Database::create(path).with_context(|| format!("failed to open header db at {}", path.display()))?;

		// Ensure table exists
		let txn = db
			.begin_write()
			.context("failed to begin write txn for table creation")?;
		{
			let _t = txn.open_table(HEADERS).context("failed to open headers table")?;
		}
		txn.commit().context("failed to commit table creation")?;

		Ok(Self { db })
	}
}

impl HeaderStoreBackend for RedbHeaderStore {
	fn persist_header(&self, header: &BlockHeader, height: u32) -> Result<()> {
		let bytes = header.to_bytes();
		let arr: &[u8; HEADER_SIZE] = bytes
			.as_slice()
			.try_into()
			.context("header serialization produced wrong size")?;

		let txn = self.db.begin_write().context("failed to begin write txn")?;
		{
			let mut table = txn.open_table(HEADERS).context("failed to open headers table")?;
			table.insert(height, arr).context("failed to insert header")?;
		}
		txn.commit().context("failed to commit header persist")?;
		Ok(())
	}

	fn persist_headers(&self, headers: &[(BlockHeader, u32)]) -> Result<()> {
		if headers.is_empty() {
			return Ok(());
		}

		let txn = self.db.begin_write().context("failed to begin write txn for batch")?;
		{
			let mut table = txn.open_table(HEADERS).context("failed to open headers table")?;

			for (header, height) in headers {
				let bytes = header.to_bytes();
				let arr: &[u8; HEADER_SIZE] = bytes
					.as_slice()
					.try_into()
					.context("header serialization produced wrong size")?;
				table.insert(*height, arr).context("failed to insert header in batch")?;
			}
		}
		txn.commit().context("failed to commit batch persist")?;
		Ok(())
	}

	fn load_all(&self) -> Result<Vec<(BlockHeader, u32)>> {
		let txn = self.db.begin_read().context("failed to begin read txn")?;
		let table = txn.open_table(HEADERS).context("failed to open headers table")?;

		let len = table.len().context("failed to get table length")?;

		if len > MAX_LOADABLE_HEADERS {
			error!(
				count = len,
				max = MAX_LOADABLE_HEADERS,
				"header database has unreasonable entry count"
			);
			bail!("header database has {len} entries, exceeds safety limit of {MAX_LOADABLE_HEADERS}");
		}

		#[allow(clippy::cast_possible_truncation)] // capped by MAX_LOADABLE_HEADERS which fits in usize
		let mut result = Vec::with_capacity(len as usize);

		for entry in table.iter().context("failed to iterate headers table")? {
			let (height_guard, value_guard) = entry.context("failed to read header entry")?;
			let height = height_guard.value();
			let bytes: &[u8; HEADER_SIZE] = value_guard.value();

			let header = BlockHeader::from_bytes(bytes)
				.with_context(|| format!("failed to decode header at height {height}"))?;

			// Verify height continuity: each entry must be at the expected index
			#[allow(clippy::cast_possible_truncation)] // result.len() bounded by table size
			let expected_height = result.len() as u32;
			if height != expected_height {
				error!(expected_height, height, "height gap in header database");
				bail!("height gap in header database: expected {expected_height}, got {height}");
			}

			result.push((header, height));
		}

		Ok(result)
	}

	fn count(&self) -> Result<u64> {
		let txn = self.db.begin_read().context("failed to begin read txn for count")?;
		let table = txn
			.open_table(HEADERS)
			.context("failed to open headers table for count")?;
		table.len().context("failed to get table length")
	}

	fn clear(&self) -> Result<()> {
		let txn = self.db.begin_write().context("failed to begin write txn for clear")?;
		txn.delete_table(HEADERS).context("failed to delete headers table")?;
		// Re-create empty table so subsequent operations don't fail
		txn.open_table(HEADERS)
			.context("failed to recreate headers table after clear")?;
		txn.commit().context("failed to commit clear")?;
		Ok(())
	}
}
