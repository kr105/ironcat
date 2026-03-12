// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use rkyv::{Archive, Deserialize, Serialize};
use std::{net::IpAddr, path::Path};

use super::{atomic_write, checksummed_encode};

/// On-disk format for the peer database
#[derive(Debug, Archive, Serialize, Deserialize)]
pub struct PeerDb {
	/// Schema version for future migrations
	pub version: u32,
	/// Known good peers
	pub peers: Vec<SavedPeer>,
}

/// A single peer entry for persistence
#[derive(Debug, Archive, Serialize, Deserialize)]
pub struct SavedPeer {
	/// IP address
	pub ip: IpAddr,
	/// Listening port
	pub port: u16,
	/// Service flags (`ServiceMask::bits()`)
	pub services: u64,
	/// Unix timestamp of last message received
	pub last_seen: u64,
	/// User agent string
	pub user_agent: String,
	/// Last known block height
	pub height: i32,
}

/// Saves a `PeerDb` to disk with checksum and atomic write
pub fn save(path: &Path, db: &PeerDb) -> Result<()> {
	let data = checksummed_encode(db)?;
	atomic_write(path, &data)
}
