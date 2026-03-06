// SPDX-License-Identifier: Apache-2.0

pub mod bans;
pub mod block_store;
pub mod header_store_backend;
pub mod header_store_redb;
pub mod peers;

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
use tracing::{debug, warn};

use crate::nodes::NodeManager;

/// Save interval (15 minutes)
const SAVE_INTERVAL_SECS: u64 = 15 * 60;

/// SHA256 checksum length in bytes
const CHECKSUM_LEN: usize = 32;

/// Serializes a value with bincode and prepends a SHA256 checksum
pub fn checksummed_encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
	let payload = bincode::serialize(value).context("failed to serialize data")?;
	let checksum = Sha256::digest(&payload);

	let mut out = Vec::with_capacity(CHECKSUM_LEN.saturating_add(payload.len()));
	out.extend_from_slice(&checksum);
	out.extend_from_slice(&payload);
	Ok(out)
}

/// Validates SHA256 checksum and deserializes bincode payload
pub fn checksummed_decode<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
	if data.len() < CHECKSUM_LEN {
		bail!("file too short ({} bytes, need at least {CHECKSUM_LEN})", data.len());
	}

	let (stored_checksum, payload) = data.split_at(CHECKSUM_LEN);
	let computed = Sha256::digest(payload);

	if computed.as_slice() != stored_checksum {
		bail!("checksum mismatch");
	}

	bincode::deserialize(payload).context("failed to deserialize data")
}

/// Atomically writes data to a file (write to .tmp, then rename)
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
	let tmp_path = path.with_extension("tmp");
	std::fs::write(&tmp_path, data).context("failed to write temp file")?;
	std::fs::rename(&tmp_path, path).context("failed to rename temp file")?;
	Ok(())
}

/// Loads a checksummed file, returning None if the file doesn't exist
///
/// Logs a warning and returns None on checksum or deserialization failure
pub fn load_file<T: DeserializeOwned>(path: &Path) -> Option<T> {
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
