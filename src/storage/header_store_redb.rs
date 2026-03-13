// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result, bail};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use tracing::{error, warn};

use crate::difficulty::U256;
use crate::types::block::{BlockHeader, HEADER_SIZE};
use crate::types::hash::Hash256;

use super::header_store_backend::HeaderStoreBackend;

/// Value size: 80 bytes header + 4 bytes height LE + 32 bytes chainwork LE = 116 bytes
const ENTRY_SIZE: usize = HEADER_SIZE + 4 + 32;

/// Table mapping block hash (32 bytes) to serialized (header + height + chainwork)
const HEADERS: TableDefinition<&[u8; 32], &[u8; ENTRY_SIZE]> = TableDefinition::new("headers_v2");

/// Table storing invalid tip hashes (hash -> empty value)
const INVALID_TIPS: TableDefinition<&[u8; 32], ()> = TableDefinition::new("invalid_tips");

/// Sanity cap for header count to prevent memory exhaustion from corrupted databases
const MAX_LOADABLE_HEADERS: u64 = 10_000_000;

/// redb-backed persistent storage for block headers
///
/// Uses a table keyed by block hash. Each value encodes the 80-byte header,
/// 4-byte LE height, and 32-byte LE chainwork (116 bytes total)
pub struct RedbHeaderStore {
	db: Database,
}

impl RedbHeaderStore {
	/// Opens (or creates) a header database at the given path
	///
	/// Detects old schema (height-keyed `headers` table) and clears the
	/// database if found. The node will re-sync headers from peers
	pub fn open(path: &Path) -> Result<Self> {
		let db = super::open_or_recreate_db(path)?;

		// Detect and migrate from old height-keyed schema
		{
			let txn = db.begin_write().context("failed to begin write txn for schema check")?;

			// Remove old height-keyed "headers" table if it exists (schema migration)
			if txn
				.delete_table(TableDefinition::<u32, &[u8; HEADER_SIZE]>::new("headers"))
				.is_ok()
			{
				warn!("detected old height-keyed header schema, cleared for re-sync");
			}

			// Ensure new tables exist
			{
				let _t = txn.open_table(HEADERS).context("failed to open headers_v2 table")?;
				let _t = txn
					.open_table(INVALID_TIPS)
					.context("failed to open invalid_tips table")?;
			}
			txn.commit().context("failed to commit table creation")?;
		}

		Ok(Self { db })
	}
}

/// Encodes a header entry into the fixed-size byte array
fn encode_entry(header: &BlockHeader, height: u32, chainwork: U256) -> [u8; ENTRY_SIZE] {
	let mut buf = [0u8; ENTRY_SIZE];
	let header_bytes = header.to_bytes();
	buf[..HEADER_SIZE].copy_from_slice(&header_bytes);
	buf[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&height.to_le_bytes());

	// U256 to 32 bytes LE
	let cw_bytes = chainwork.to_little_endian();
	buf[HEADER_SIZE + 4..].copy_from_slice(&cw_bytes);
	buf
}

/// Decodes a header entry from the fixed-size byte array
fn decode_entry(hash_bytes: &[u8; 32], buf: &[u8; ENTRY_SIZE]) -> Result<(Hash256, BlockHeader, u32, U256)> {
	let hash = Hash256::from_bytes(*hash_bytes);

	#[allow(clippy::indexing_slicing)] // buf is exactly ENTRY_SIZE
	let header = BlockHeader::from_bytes(&buf[..HEADER_SIZE]).context("failed to decode header from entry")?;

	#[allow(clippy::indexing_slicing)] // buf is exactly ENTRY_SIZE
	let height = u32::from_le_bytes(
		buf[HEADER_SIZE..HEADER_SIZE + 4]
			.try_into()
			.context("failed to decode height")?,
	);

	#[allow(clippy::indexing_slicing)] // buf is exactly ENTRY_SIZE
	let cw_bytes: [u8; 32] = buf[HEADER_SIZE + 4..]
		.try_into()
		.context("failed to decode chainwork bytes")?;
	let chainwork = U256::from_little_endian(&cw_bytes);

	Ok((hash, header, height, chainwork))
}

impl HeaderStoreBackend for RedbHeaderStore {
	fn persist_header(&self, hash: &Hash256, header: &BlockHeader, height: u32, chainwork: U256) -> Result<()> {
		let entry = encode_entry(header, height, chainwork);

		let txn = self.db.begin_write().context("failed to begin write txn")?;
		{
			let mut table = txn.open_table(HEADERS).context("failed to open headers table")?;
			table
				.insert(hash.as_bytes(), &entry)
				.context("failed to insert header")?;
		}
		txn.commit().context("failed to commit header persist")?;
		Ok(())
	}

	fn persist_headers(&self, headers: &[(Hash256, BlockHeader, u32, U256)]) -> Result<()> {
		if headers.is_empty() {
			return Ok(());
		}

		let txn = self.db.begin_write().context("failed to begin write txn for batch")?;
		{
			let mut table = txn.open_table(HEADERS).context("failed to open headers table")?;
			for (hash, header, height, chainwork) in headers {
				let entry = encode_entry(header, *height, *chainwork);
				table
					.insert(hash.as_bytes(), &entry)
					.context("failed to insert header in batch")?;
			}
		}
		txn.commit().context("failed to commit batch persist")?;
		Ok(())
	}

	fn load_all(&self) -> Result<Vec<(Hash256, BlockHeader, u32, U256)>> {
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
			let (key_guard, value_guard) = entry.context("failed to read header entry")?;
			let hash_bytes: &[u8; 32] = key_guard.value();
			let buf: &[u8; ENTRY_SIZE] = value_guard.value();
			result.push(decode_entry(hash_bytes, buf)?);
		}

		Ok(result)
	}

	fn persist_invalid_tip(&self, hash: &Hash256) -> Result<()> {
		let txn = self
			.db
			.begin_write()
			.context("failed to begin write txn for invalid tip")?;
		{
			let mut table = txn
				.open_table(INVALID_TIPS)
				.context("failed to open invalid_tips table")?;
			table
				.insert(hash.as_bytes(), ())
				.context("failed to insert invalid tip")?;
		}
		txn.commit().context("failed to commit invalid tip persist")?;
		Ok(())
	}

	fn load_invalid_tips(&self) -> Result<Vec<Hash256>> {
		let txn = self
			.db
			.begin_read()
			.context("failed to begin read txn for invalid tips")?;
		let table = txn
			.open_table(INVALID_TIPS)
			.context("failed to open invalid_tips table")?;

		let mut result = Vec::new();
		for entry in table.iter().context("failed to iterate invalid_tips table")? {
			let (key_guard, _) = entry.context("failed to read invalid tip entry")?;
			let hash_bytes: &[u8; 32] = key_guard.value();
			result.push(Hash256::from_bytes(*hash_bytes));
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
		if let Err(e) = txn.delete_table(HEADERS) {
			warn!(error = %e, "failed to delete headers table during clear (may not exist)");
		}
		if let Err(e) = txn.delete_table(INVALID_TIPS) {
			warn!(error = %e, "failed to delete invalid_tips table during clear (may not exist)");
		}
		// Re-create empty tables so subsequent operations don't fail
		txn.open_table(HEADERS)
			.context("failed to recreate headers table after clear")?;
		txn.open_table(INVALID_TIPS)
			.context("failed to recreate invalid_tips table after clear")?;
		txn.commit().context("failed to commit clear")?;
		Ok(())
	}
}
