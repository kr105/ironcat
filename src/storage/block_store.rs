// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use tracing::{debug, info, warn};

use crate::types::hash::{HASH_LEN, Hash256};

/// Catcoin mainnet magic bytes used as flat file block separator
const BLOCK_MAGIC: [u8; 4] = [0xFC, 0xC1, 0xB7, 0xDC];

/// Default maximum flat file size before rotation (256 MB)
const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Size of the block entry prefix: magic (4) + size (4)
const ENTRY_PREFIX_SIZE: u64 = 8;

/// Table mapping block hash (32 bytes) to location (`file_number`: u16, offset: u32, size: u32)
const BLOCK_INDEX: TableDefinition<&[u8; 32], (u16, u32, u32)> = TableDefinition::new("block_index");

/// Location of a block in the flat file storage
pub struct BlockLocation {
	/// Which blkNNNNN.dat file the block lives in
	pub file_number: u16,
	/// Byte offset within the file (points to the magic prefix)
	pub offset: u32,
	/// Size of the raw block data (excluding the 8-byte prefix)
	pub size: u32,
}

/// Pending index entry waiting to be flushed to redb
struct PendingEntry {
	hash: [u8; HASH_LEN],
	file_number: u16,
	offset: u32,
	size: u32,
}

/// Flat-file block storage with redb index
///
/// Blocks are appended to `blkNNNNN.dat` files with a magic+size prefix.
/// A redb table maps block hashes to their file location for O(1) lookup.
/// Writes are batched: flat file writes happen immediately but fsync and
/// redb index updates are deferred until `flush()` is called
pub struct BlockStore {
	db: Database,
	blocks_dir: PathBuf,
	max_file_size: u64,
	current_file: Mutex<(u16, File, u64)>,
	/// Pending index entries not yet committed to redb
	pending: Mutex<Vec<PendingEntry>>,
}

impl BlockStore {
	/// Opens or creates a block store at the given data directory
	///
	/// Blocks go in `datadir/blocks/`, index db is `datadir/index.redb`
	pub fn open(datadir: &Path) -> Result<Self> {
		Self::open_with_max_file_size(datadir, DEFAULT_MAX_FILE_SIZE)
	}

	/// Opens or creates a block store with a custom max file size (for testing)
	pub fn open_with_max_file_size(datadir: &Path, max_size: u64) -> Result<Self> {
		if max_size > u64::from(u32::MAX) {
			anyhow::bail!("max_file_size exceeds u32::MAX");
		}

		let blocks_dir = datadir.join("blocks");
		fs::create_dir_all(&blocks_dir)
			.with_context(|| format!("failed to create blocks dir at {}", blocks_dir.display()))?;

		let db_path = datadir.join("index.redb");
		let db = super::open_or_recreate_db(&db_path)?;

		// Ensure table exists
		let txn = db
			.begin_write()
			.context("failed to begin write txn for table creation")?;
		{
			let _t = txn
				.open_table(BLOCK_INDEX)
				.context("failed to open block_index table")?;
		}
		txn.commit().context("failed to commit table creation")?;

		// Find the highest file number from the index and verify that the
		// flat files on disk are consistent with what the index expects.
		// If the index says file N should have at least X bytes but the
		// actual file is smaller (or missing), the index is stale
		let file_number = Self::verify_or_clear_index(&db, &blocks_dir)?;

		// Open the current file and get its size
		let file_path = blocks_dir.join(format!("blk{file_number:05}.dat"));
		let file = OpenOptions::new()
			.append(true)
			.create(true)
			.open(&file_path)
			.with_context(|| format!("failed to open flat file {}", file_path.display()))?;

		let offset = file.metadata().context("failed to get flat file metadata")?.len();

		info!(
			file_number,
			offset,
			blocks_dir = %blocks_dir.display(),
			"Block store opened"
		);

		Ok(Self {
			db,
			blocks_dir,
			max_file_size: max_size,
			current_file: Mutex::new((file_number, file, offset)),
			pending: Mutex::new(Vec::new()),
		})
	}

	/// Stores a raw block in the flat file and queues an index update
	///
	/// The block is written to the flat file immediately (buffered by the OS)
	/// but the redb index update and fsync are deferred until `flush()` is
	/// called. Returns Ok(()) if the block is already stored (idempotent).
	/// Includes a redb read for dedup -- use `store_block_unchecked` when
	/// the caller already guarantees uniqueness (e.g. via in-memory tracking)
	pub fn store_block(&self, hash: &Hash256, raw_bytes: &[u8]) -> Result<()> {
		// Idempotency: check committed index
		{
			let txn = self
				.db
				.begin_read()
				.context("failed to begin read txn for block dedup check")?;
			let table = txn
				.open_table(BLOCK_INDEX)
				.context("failed to open block_index for dedup check")?;
			if table
				.get(hash.as_bytes())
				.context("failed to read block_index for dedup check")?
				.is_some()
			{
				return Ok(());
			}
		}

		self.store_block_unchecked(hash, raw_bytes)
	}

	/// Stores a raw block without checking the redb index for duplicates
	///
	/// Use when the caller already guarantees this block hasn't been stored
	/// (e.g. the download manager tracks stored hashes in memory). Skipping
	/// the redb read transaction per block significantly improves throughput
	pub fn store_block_unchecked(&self, hash: &Hash256, raw_bytes: &[u8]) -> Result<()> {
		let mut guard = self.current_file.lock();

		let (ref mut file_number, ref mut file, ref mut offset) = *guard;

		// Check if we need to rotate
		let entry_size = ENTRY_PREFIX_SIZE.saturating_add(raw_bytes.len() as u64);

		if *offset > 0 && offset.saturating_add(entry_size) > self.max_file_size {
			let next = file_number
				.checked_add(1)
				.ok_or_else(|| anyhow::anyhow!("block file number overflow"))?;
			let next_path = self.blocks_dir.join(format!("blk{next:05}.dat"));
			let next_file = OpenOptions::new()
				.append(true)
				.create(true)
				.open(&next_path)
				.with_context(|| format!("failed to open next flat file {}", next_path.display()))?;

			info!(from = *file_number, to = next, "Rotating block flat file");

			*file_number = next;
			*file = next_file;
			*offset = 0;
		}

		// Record offset before writing
		#[allow(clippy::cast_possible_truncation)] // flat files capped at max_file_size which fits u32
		let block_offset = *offset as u32;
		#[allow(clippy::cast_possible_truncation)] // individual blocks won't exceed u32 in practice
		let block_size = raw_bytes.len() as u32;

		// Write: magic(4) + size(4 LE) + raw_block_bytes (no fsync -- deferred to flush)
		file.write_all(&BLOCK_MAGIC).context("failed to write block magic")?;
		file.write_all(&block_size.to_le_bytes())
			.context("failed to write block size")?;
		file.write_all(raw_bytes).context("failed to write block data")?;

		// Queue index entry for batch commit in flush()
		let current_fn = *file_number;
		self.pending.lock().push(PendingEntry {
			hash: *hash.as_bytes(),
			file_number: current_fn,
			offset: block_offset,
			size: block_size,
		});

		// Update offset
		*offset = offset.saturating_add(entry_size);

		drop(guard);

		debug!(
			%hash,
			file_number = current_fn,
			block_offset,
			block_size,
			"Block written to flat file"
		);

		Ok(())
	}

	/// Flushes pending writes: fsyncs the flat file and commits all queued
	/// index entries to redb in a single transaction
	///
	/// Safe to call when there are no pending entries (no-op)
	pub fn flush(&self) -> Result<()> {
		let entries: Vec<PendingEntry> = {
			let mut pending = self.pending.lock();
			if pending.is_empty() {
				return Ok(());
			}
			std::mem::take(&mut *pending)
		};

		let count = entries.len();

		// Fsync the flat file first -- if we crash after this but before the
		// index commit, the blocks are on disk but unindexed (will be re-downloaded)
		let result = (|| -> Result<()> {
			{
				let guard = self.current_file.lock();
				guard.1.sync_all().context("failed to fsync flat file during flush")?;
			}

			// Batch commit all pending entries in a single redb transaction
			let txn = self.db.begin_write().context("failed to begin write txn for flush")?;
			{
				let mut table = txn
					.open_table(BLOCK_INDEX)
					.context("failed to open block_index table for flush")?;
				for entry in &entries {
					table
						.insert(&entry.hash, (entry.file_number, entry.offset, entry.size))
						.context("failed to insert block index entry during flush")?;
				}
			}
			txn.commit().context("failed to commit flush")?;
			Ok(())
		})();

		if let Err(e) = result {
			// Restore entries so they can be retried on the next flush
			self.pending.lock().extend(entries);
			return Err(e);
		}

		debug!(count, "Flushed block index entries");
		Ok(())
	}

	/// Returns the number of pending (unflushed) index entries
	pub fn pending_count(&self) -> usize {
		self.pending.lock().len()
	}

	/// Checks if a block is already stored in the index
	pub fn has_block(&self, hash: &Hash256) -> bool {
		self.get_location(hash).is_some()
	}

	/// Looks up a block's file location from the index
	pub fn get_location(&self, hash: &Hash256) -> Option<BlockLocation> {
		let txn = match self.db.begin_read() {
			Ok(t) => t,
			Err(e) => {
				warn!(error = %e, "Failed to begin read txn for block lookup");
				return None;
			}
		};

		let table = match txn.open_table(BLOCK_INDEX) {
			Ok(t) => t,
			Err(e) => {
				warn!(error = %e, "Failed to open block_index table for lookup");
				return None;
			}
		};

		match table.get(hash.as_bytes()) {
			Ok(Some(guard)) => {
				let (file_number, offset, size) = guard.value();
				Some(BlockLocation {
					file_number,
					offset,
					size,
				})
			}
			Ok(None) => None,
			Err(e) => {
				warn!(error = %e, %hash, "Failed to read block index entry");
				None
			}
		}
	}

	/// Loads raw block bytes from the flat file by hash
	///
	/// Looks up the block's location in the index, seeks past the 8-byte
	/// magic+size prefix, and reads the raw block data
	pub fn load_block(&self, hash: &Hash256) -> Result<Vec<u8>> {
		let loc = self
			.get_location(hash)
			.ok_or_else(|| anyhow::anyhow!("block {hash} not found in index"))?;

		let file_path = self.blocks_dir.join(format!("blk{:05}.dat", loc.file_number));
		let mut file =
			File::open(&file_path).with_context(|| format!("failed to open block file {}", file_path.display()))?;

		// Seek past the magic+size prefix to the actual block data
		#[allow(clippy::arithmetic_side_effects)] // u32 offset + 8 fits u64
		let data_offset = u64::from(loc.offset) + ENTRY_PREFIX_SIZE;
		file.seek(SeekFrom::Start(data_offset))
			.context("failed to seek in block file")?;

		let mut buf = vec![0u8; loc.size as usize];
		file.read_exact(&mut buf)
			.context("failed to read block data from flat file")?;

		Ok(buf)
	}

	/// Loads and deserializes a block from the flat file by hash
	///
	/// Wraps `load_block` with `Block::from_bytes`. No re-validation is
	/// performed since these blocks were already validated when first stored
	pub fn load_and_deserialize(&self, hash: &Hash256) -> Result<crate::types::block::Block> {
		let raw = self.load_block(hash)?;
		crate::types::block::Block::from_bytes(&raw).with_context(|| format!("failed to deserialize block {hash}"))
	}

	/// Returns all block hashes currently committed in the redb index
	///
	/// Used at startup to populate in-memory tracking so the download
	/// manager can skip already-stored blocks without per-height redb reads
	pub fn all_indexed_hashes(&self) -> Result<HashSet<Hash256>> {
		let txn = self.db.begin_read().context("failed to begin read txn for hash scan")?;
		let table = txn
			.open_table(BLOCK_INDEX)
			.context("failed to open block_index for hash scan")?;

		let mut hashes = HashSet::new();
		for entry in table.iter().context("failed to iterate block_index")? {
			let (key, _value) = entry.context("failed to read block_index entry during hash scan")?;
			hashes.insert(Hash256::from_bytes(*key.value()));
		}

		info!(count = hashes.len(), "loaded block index hashes into memory");
		Ok(hashes)
	}

	/// Verifies the block index against flat files on disk
	///
	/// Scans the index to find the expected minimum size of each flat file
	/// (max offset + prefix + block size per file). If any flat file is
	/// smaller than what the index expects, the index is stale and gets
	/// cleared. Returns the highest valid file number, or 0 if cleared
	fn verify_or_clear_index(db: &Database, blocks_dir: &Path) -> Result<u16> {
		let txn = db.begin_read().context("failed to begin read txn for index verify")?;
		let table = txn
			.open_table(BLOCK_INDEX)
			.context("failed to open block_index for verify")?;

		// Track the expected minimum size per file number:
		// offset + ENTRY_PREFIX_SIZE + block_size = end of that block's data
		let mut file_expected_sizes: HashMap<u16, u64> = HashMap::new();

		for entry in table.iter().context("failed to iterate block_index for verify")? {
			let (_key, value) = entry.context("failed to read block_index entry during verify")?;
			let (fnum, offset, size) = value.value();
			let end = u64::from(offset)
				.saturating_add(ENTRY_PREFIX_SIZE)
				.saturating_add(u64::from(size));
			let current = file_expected_sizes.entry(fnum).or_insert(0);
			if end > *current {
				*current = end;
			}
		}

		drop(table);
		drop(txn);

		if file_expected_sizes.is_empty() {
			return Ok(0);
		}

		// Check each referenced file against its expected minimum size
		for (&fnum, &expected_size) in &file_expected_sizes {
			let path = blocks_dir.join(format!("blk{fnum:05}.dat"));
			let actual_size = path.metadata().map(|m| m.len()).unwrap_or(0);
			if actual_size < expected_size {
				warn!(
					file_number = fnum,
					expected_size, actual_size, "flat file smaller than index expects, clearing stale index"
				);
				Self::clear_index(db)?;
				return Ok(0);
			}
		}

		// All files check out, return the highest file number
		let max_file = file_expected_sizes.keys().copied().max().unwrap_or(0);
		Ok(max_file)
	}

	/// Removes all entries from the block index
	///
	/// Drops and recreates the table in a single transaction
	fn clear_index(db: &Database) -> Result<()> {
		let txn = db.begin_write().context("failed to begin write txn for index clear")?;
		txn.delete_table(BLOCK_INDEX)
			.context("failed to delete block_index table")?;
		txn.open_table(BLOCK_INDEX)
			.context("failed to recreate block_index table")?;
		txn.commit().context("failed to commit index clear")?;
		info!("cleared stale block index");
		Ok(())
	}
}
