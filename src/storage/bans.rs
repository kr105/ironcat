// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, path::Path};

use crate::utils::unix_now;

use super::{atomic_write, checksummed_encode};

/// On-disk format for the ban database
#[derive(Debug, Serialize, Deserialize)]
pub struct BanDb {
	/// Schema version for future migrations
	pub version: u32,
	/// Ban entries
	pub bans: Vec<SavedBan>,
}

impl BanDb {
	/// Returns only bans that haven't expired yet
	pub fn active_bans(&self) -> Vec<&SavedBan> {
		let now = unix_now();
		self.bans.iter().filter(|b| b.expires > now).collect()
	}
}

/// A single ban entry for persistence
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedBan {
	/// Banned IP address
	pub ip: IpAddr,
	/// Human-readable ban reason
	pub reason: String,
	/// Unix timestamp when the ban was created
	pub created: u64,
	/// Unix timestamp when the ban expires
	pub expires: u64,
}

/// Saves a `BanDb` to disk with checksum and atomic write
pub fn save(path: &Path, db: &BanDb) -> Result<()> {
	let data = checksummed_encode(db)?;
	atomic_write(path, &data)
}
