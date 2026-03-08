// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use redb::{Database, ReadableTable, TableDefinition};
use tracing::{debug, info, warn};

use crate::types::hash::{Hash256, HASH_LEN};

use super::undo::BlockUndo;

/// Catcoin mainnet magic bytes used as flat file entry separator
const UNDO_MAGIC: [u8; 4] = [0xFC, 0xC1, 0xB7, 0xDC];

/// Default maximum flat file size before rotation (256 MB)
const DEFAULT_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Size of the entry prefix: magic (4) + size (4)
const ENTRY_PREFIX_SIZE: u64 = 8;

/// Table mapping block hash (32 bytes) to undo location (`file_number`: u16, offset: u32, size: u32)
pub const UNDO_INDEX: TableDefinition<&[u8; 32], (u16, u32, u32)> = TableDefinition::new("undo_index");

/// Location of an undo entry in the flat file storage
///
/// Returned by `store_undo_to_file` so the caller can write the index
/// entry inside its own redb transaction (atomic with UTXO mutations)
pub struct UndoLocation {
	/// Hash of the block this undo data belongs to
	pub hash: [u8; HASH_LEN],
	/// Which revNNNNN.dat file
	pub file_number: u16,
	/// Byte offset within the file
	pub offset: u32,
	/// Size of the serialized undo data (excluding prefix)
	pub size: u32,
}

/// Flat-file undo data storage with redb index
///
/// Undo records are appended to `revNNNNN.dat` files with a magic+size prefix.
/// The redb index entries are written by `ChainState` inside its own write
/// transaction for atomicity with UTXO mutations.
///
/// The Database is owned externally (by `ChainState`) and passed to methods
/// that need it. This allows sharing a single redb Database across the
/// UTXO set, undo index, and metadata tables
pub struct UndoStore {
	blocks_dir: PathBuf,
	max_file_size: u64,
	current_file: Mutex<(u16, File, u64)>,
}

impl UndoStore {
	/// Opens or creates an undo store at the given data directory
	///
	/// The caller must provide a redb Database that already has the
	/// `UNDO_INDEX` table created. Undo files go in `datadir/blocks/`
	pub fn open(datadir: &Path, db: &Database) -> Result<Self> {
		Self::open_with_max_file_size(datadir, db, DEFAULT_MAX_FILE_SIZE)
	}

	/// Opens or creates an undo store with a custom max file size (for testing)
	pub fn open_with_max_file_size(datadir: &Path, db: &Database, max_size: u64) -> Result<Self> {
		if max_size > u64::from(u32::MAX) {
			anyhow::bail!("max_file_size exceeds u32::MAX");
		}

		let blocks_dir = datadir.join("blocks");
		fs::create_dir_all(&blocks_dir)
			.with_context(|| format!("failed to create blocks dir at {}", blocks_dir.display()))?;

		// Find the highest file number from the index and verify that the
		// flat files on disk are consistent with what the index expects
		let file_number = Self::verify_or_clear_index(db, &blocks_dir)?;

		// Open the current file and get its size
		let file_path = blocks_dir.join(format!("rev{file_number:05}.dat"));
		let file = OpenOptions::new()
			.append(true)
			.create(true)
			.open(&file_path)
			.with_context(|| format!("failed to open undo flat file {}", file_path.display()))?;

		let offset = file.metadata().context("failed to get undo flat file metadata")?.len();

		info!(
			file_number,
			offset,
			blocks_dir = %blocks_dir.display(),
			"Undo store opened"
		);

		Ok(Self {
			blocks_dir,
			max_file_size: max_size,
			current_file: Mutex::new((file_number, file, offset)),
		})
	}

	/// Writes undo data to the flat file and returns its location
	///
	/// The caller is responsible for writing the index entry into redb
	/// inside its own write transaction (for atomicity with UTXO mutations).
	/// No fsync is performed here -- call `fsync()` after the redb commit
	pub fn store_undo_to_file(&self, block_hash: &Hash256, undo: &BlockUndo) -> Result<UndoLocation> {
		let raw_bytes = undo.to_bytes();
		let mut guard = self.current_file.lock();

		let (ref mut file_number, ref mut file, ref mut offset) = *guard;

		// Check if we need to rotate
		let entry_size = ENTRY_PREFIX_SIZE.saturating_add(raw_bytes.len() as u64);

		if *offset > 0 && offset.saturating_add(entry_size) > self.max_file_size {
			let next = file_number
				.checked_add(1)
				.ok_or_else(|| anyhow::anyhow!("undo file number overflow"))?;
			let next_path = self.blocks_dir.join(format!("rev{next:05}.dat"));
			let next_file = OpenOptions::new()
				.append(true)
				.create(true)
				.open(&next_path)
				.with_context(|| format!("failed to open next undo flat file {}", next_path.display()))?;

			info!(from = *file_number, to = next, "Rotating undo flat file");

			*file_number = next;
			*file = next_file;
			*offset = 0;
		}

		// Record offset before writing
		#[allow(clippy::cast_possible_truncation)] // flat files capped at max_file_size which fits u32
		let entry_offset = *offset as u32;
		#[allow(clippy::cast_possible_truncation)] // individual undo records won't exceed u32 in practice
		let entry_size_u32 = raw_bytes.len() as u32;

		// Write: magic(4) + size(4 LE) + undo_bytes
		file.write_all(&UNDO_MAGIC).context("failed to write undo magic")?;
		file.write_all(&entry_size_u32.to_le_bytes())
			.context("failed to write undo size")?;
		file.write_all(&raw_bytes).context("failed to write undo data")?;

		let current_fn = *file_number;

		// Update offset
		*offset = offset.saturating_add(entry_size);

		drop(guard);

		debug!(
			hash = %block_hash,
			file_number = current_fn,
			entry_offset,
			entry_size = entry_size_u32,
			"Undo data written to flat file"
		);

		Ok(UndoLocation {
			hash: *block_hash.as_bytes(),
			file_number: current_fn,
			offset: entry_offset,
			size: entry_size_u32,
		})
	}

	/// Fsyncs the current flat file to ensure undo data is durable on disk
	pub fn fsync(&self) -> Result<()> {
		let guard = self.current_file.lock();
		guard.1.sync_all().context("failed to fsync undo flat file")
	}

	/// Loads undo data for a block from the flat file
	///
	/// Looks up the index to find the file location, seeks to the data
	/// (past the 8-byte magic+size prefix), and deserializes
	pub fn load_undo(&self, db: &Database, block_hash: &Hash256) -> Result<BlockUndo> {
		let (file_number, entry_offset, size) = {
			let txn = db.begin_read().context("failed to begin read txn for undo lookup")?;
			let table = txn
				.open_table(UNDO_INDEX)
				.context("failed to open undo_index for lookup")?;
			let guard = table
				.get(block_hash.as_bytes())
				.context("failed to read undo_index entry")?
				.ok_or_else(|| anyhow::anyhow!("no undo data found for block {block_hash}"))?;
			guard.value()
		};

		let file_path = self.blocks_dir.join(format!("rev{file_number:05}.dat"));
		let mut file =
			File::open(&file_path).with_context(|| format!("failed to open undo file {}", file_path.display()))?;

		// Seek past the magic+size prefix to the actual data
		#[allow(clippy::arithmetic_side_effects)] // u32 offset + 8 fits u64
		let data_offset = u64::from(entry_offset) + ENTRY_PREFIX_SIZE;
		file.seek(SeekFrom::Start(data_offset))
			.context("failed to seek in undo file")?;

		let mut buf = vec![0u8; size as usize];
		file.read_exact(&mut buf)
			.context("failed to read undo data from flat file")?;

		BlockUndo::from_bytes(&buf).context("failed to deserialize undo data")
	}

	/// Verifies the undo index against flat files on disk
	///
	/// Scans the index to find the expected minimum size of each flat file.
	/// If any flat file is smaller than what the index expects, the index
	/// is stale and gets cleared. Returns the highest valid file number,
	/// or 0 if cleared
	fn verify_or_clear_index(db: &Database, blocks_dir: &Path) -> Result<u16> {
		let txn = db
			.begin_read()
			.context("failed to begin read txn for undo index verify")?;
		let table = txn
			.open_table(UNDO_INDEX)
			.context("failed to open undo_index for verify")?;

		// Track the expected minimum size per file number:
		// offset + ENTRY_PREFIX_SIZE + data_size = end of that entry's data
		let mut file_expected_sizes: HashMap<u16, u64> = HashMap::new();

		for entry in table.iter().context("failed to iterate undo_index for verify")? {
			let (_key, value) = entry.context("failed to read undo_index entry during verify")?;
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
			let path = blocks_dir.join(format!("rev{fnum:05}.dat"));
			let actual_size = path.metadata().map(|m| m.len()).unwrap_or(0);
			if actual_size < expected_size {
				warn!(
					file_number = fnum,
					expected_size, actual_size, "undo flat file smaller than index expects, clearing stale index"
				);
				Self::clear_index(db)?;
				return Ok(0);
			}
		}

		// All files check out, return the highest file number
		let max_file = file_expected_sizes.keys().copied().max().unwrap_or(0);
		Ok(max_file)
	}

	/// Removes all entries from the undo index
	///
	/// Drops and recreates the table in a single transaction
	fn clear_index(db: &Database) -> Result<()> {
		let txn = db
			.begin_write()
			.context("failed to begin write txn for undo index clear")?;
		txn.delete_table(UNDO_INDEX)
			.context("failed to delete undo_index table")?;
		txn.open_table(UNDO_INDEX)
			.context("failed to recreate undo_index table")?;
		txn.commit().context("failed to commit undo index clear")?;
		info!("cleared stale undo index");
		Ok(())
	}
}
