// SPDX-License-Identifier: Apache-2.0

pub mod bans;
pub mod block_store;
pub mod header_store_backend;
pub mod header_store_redb;
pub mod peers;

use anyhow::{Context, Result, bail};
use redb::Database;
use rkyv::api::high::{HighSerializer, HighValidator};
use rkyv::bytecheck::CheckBytes;
use rkyv::rancor::Strategy;
use rkyv::ser::allocator::ArenaHandle;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
use tracing::{debug, warn};

use crate::nodes::NodeManager;

/// Save interval (15 minutes)
const SAVE_INTERVAL_SECS: u64 = 15 * 60;

/// SHA256 checksum length in bytes
const CHECKSUM_LEN: usize = 32;

/// Serializes a value with rkyv and prepends a SHA256 checksum
pub fn checksummed_encode<T>(value: &T) -> Result<Vec<u8>>
where
	T: for<'a> RkyvSerialize<HighSerializer<AlignedVec, ArenaHandle<'a>, rkyv::rancor::Error>>,
{
	let payload = rkyv::to_bytes::<rkyv::rancor::Error>(value)
		.map_err(|e| anyhow::anyhow!("{e}"))
		.context("failed to serialize data")?;
	let checksum = Sha256::digest(&payload);

	let mut out = Vec::with_capacity(CHECKSUM_LEN.saturating_add(payload.len()));
	out.extend_from_slice(&checksum);
	out.extend_from_slice(&payload);
	Ok(out)
}

/// Validates SHA256 checksum and deserializes rkyv payload
pub fn checksummed_decode<T>(data: &[u8]) -> Result<T>
where
	T: Archive,
	T::Archived: for<'a> CheckBytes<HighValidator<'a, rkyv::rancor::Error>>
		+ RkyvDeserialize<T, Strategy<rkyv::de::Pool, rkyv::rancor::Error>>,
{
	if data.len() < CHECKSUM_LEN {
		bail!("file too short ({} bytes, need at least {CHECKSUM_LEN})", data.len());
	}

	let (stored_checksum, payload) = data.split_at(CHECKSUM_LEN);
	let computed = Sha256::digest(payload);

	if computed.as_slice() != stored_checksum {
		bail!("checksum mismatch");
	}

	rkyv::from_bytes::<T, rkyv::rancor::Error>(payload)
		.map_err(|e| anyhow::anyhow!("{e}"))
		.context("failed to deserialize data")
}

/// Atomically writes data to a file (write to .tmp, then rename)
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
	let tmp_path = path.with_extension("tmp");
	std::fs::write(&tmp_path, data).context("failed to write temp file")?;
	std::fs::rename(&tmp_path, path).context("failed to rename temp file")?;
	Ok(())
}

/// Opens or creates a redb database, recreating it when the file is
/// corrupt or uses an incompatible format version
///
/// Errors that indicate a broken file (corruption, format mismatch, I/O
/// errors on the file itself) cause the old file to be deleted and a
/// fresh database to be created. The node will re-sync the lost data.
///
/// Errors that are *not* file problems (lock contention, permission
/// denied, disk full) are returned as-is since deleting wouldn't help
pub fn open_or_recreate_db(path: &Path) -> Result<Database> {
	match Database::create(path) {
		Ok(db) => Ok(db),
		Err(e) if is_recoverable_db_error(&e) => {
			warn!(
				path = %path.display(),
				error = %e,
				"Removing unusable database, data will be re-synced"
			);
			std::fs::remove_file(path).with_context(|| format!("failed to remove broken db at {}", path.display()))?;
			Database::create(path).with_context(|| format!("failed to create fresh db at {}", path.display()))
		}
		Err(e) => Err(e).with_context(|| format!("failed to open db at {}", path.display())),
	}
}

/// Returns true if the error indicates a broken database file that can
/// be safely deleted and recreated
///
/// Permission errors, lock contention, and disk-full are NOT recoverable
/// by deletion -- the recreate would fail the same way
fn is_recoverable_db_error(e: &redb::DatabaseError) -> bool {
	use redb::{DatabaseError, StorageError};

	match e {
		// Old file format, corrupted repair session, corrupted data
		DatabaseError::UpgradeRequired(_)
		| DatabaseError::RepairAborted
		| DatabaseError::Storage(StorageError::Corrupted(_)) => true,
		// I/O error on the file itself (bad reads, truncated file, etc)
		// but NOT permission denied or disk full -- those would fail again
		DatabaseError::Storage(StorageError::Io(io_err)) => !matches!(
			io_err.kind(),
			std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::StorageFull
		),
		// Lock contention, poisoned lock, etc -- not file problems
		_ => false,
	}
}

/// Loads a checksummed file, returning None if the file doesn't exist
///
/// Logs a warning and returns None on checksum or deserialization failure
pub fn load_file<T>(path: &Path) -> Option<T>
where
	T: Archive,
	T::Archived: for<'a> CheckBytes<HighValidator<'a, rkyv::rancor::Error>>
		+ RkyvDeserialize<T, Strategy<rkyv::de::Pool, rkyv::rancor::Error>>,
{
	let data = match std::fs::read(path) {
		Ok(d) => d,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
		Err(e) => {
			warn!(path = %path.display(), error = %e, "Failed to read file");
			return None;
		}
	};

	match checksummed_decode(&data) {
		Ok(value) => Some(value),
		Err(e) => {
			warn!(path = %path.display(), error = %e, "Failed to decode file, starting fresh");
			None
		}
	}
}

/// Periodically saves peers and bans to disk
pub async fn persistence_task(node_manager: Arc<NodeManager>, datadir: std::path::PathBuf) {
	let mut interval = tokio::time::interval(std::time::Duration::from_secs(SAVE_INTERVAL_SECS));
	interval.tick().await; // skip immediate first tick

	loop {
		interval.tick().await;
		save_all(&node_manager, &datadir);
	}
}

/// Saves both peers and bans to disk
pub fn save_all(node_manager: &NodeManager, datadir: &Path) {
	let peer_db = node_manager.collect_peers_for_save();
	if let Err(e) = peers::save(&datadir.join("peers.dat"), &peer_db) {
		warn!(error = %e, "Failed to save peers.dat");
	} else {
		debug!(count = peer_db.peers.len(), "Saved peers.dat");
	}

	let ban_db = node_manager.collect_bans_for_save();
	if let Err(e) = bans::save(&datadir.join("banlist.dat"), &ban_db) {
		warn!(error = %e, "Failed to save banlist.dat");
	} else {
		debug!(count = ban_db.bans.len(), "Saved banlist.dat");
	}
}
