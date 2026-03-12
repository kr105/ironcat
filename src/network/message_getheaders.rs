// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{Context, Result, anyhow};
use byteorder::{LittleEndian, ReadBytesExt};

use super::{decode_varint, write_varint};
use crate::types::hash::{HASH_LEN, Hash256};

/// Protocol version we send in the getheaders version field
pub const GETHEADERS_VERSION: u32 = 70012;

/// Minimum locator version we accept from peers
///
/// The version field in the block locator is the negotiated protocol version.
/// We warn on versions below this but still process the message, matching the
/// reference client behavior of reading and discarding this field
pub const MIN_GETHEADERS_VERSION: u32 = 70012;

/// Maximum number of block locator hashes in a getheaders message
pub const MAX_LOCATOR_HASHES: usize = 101;

/// A `getheaders` message payload, requesting block headers from a peer
///
/// Wire format:
/// - `[4]`  version: u32 LE, always `GETHEADERS_VERSION`
/// - `[var]` `hash_count`: `VarInt`, number of locator hashes (max `MAX_LOCATOR_HASHES`)
/// - `[32 x hash_count]` block locator hashes
/// - `[32]` `hash_stop`: stop hash (zeroed means go to tip)
#[derive(Debug)]
pub struct MessageGetHeaders {
	locator_hashes: Vec<Hash256>,
	hash_stop: Hash256,
}

impl MessageGetHeaders {
	/// Creates a new `getheaders` message
	///
	/// `locator_hashes` is the block locator list and `hash_stop` is the
	/// hash to stop at (use `Hash256::ZERO` to request up to the tip)
	pub const fn new(locator_hashes: Vec<Hash256>, hash_stop: Hash256) -> Self {
		Self {
			locator_hashes,
			hash_stop,
		}
	}

	/// Returns the block locator hashes
	pub fn locator_hashes(&self) -> &[Hash256] {
		&self.locator_hashes
	}

	/// Returns the stop hash
	pub const fn hash_stop(&self) -> Hash256 {
		self.hash_stop
	}

	/// Serializes the message to wire bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		// 4 (version) + 9 (varint max) + 32 * hash_count + 32 (hash_stop)
		// locator_hashes.len() is bounded by MAX_LOCATOR_HASHES (101), no overflow possible
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 4 + 9 + self.locator_hashes.len() * HASH_LEN + HASH_LEN;
		let mut bytes = Vec::with_capacity(capacity);

		bytes.extend_from_slice(&GETHEADERS_VERSION.to_le_bytes());

		// locator_hashes.len() bounded by MAX_LOCATOR_HASHES, fits in u64
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.locator_hashes.len() as u64);

		for hash in &self.locator_hashes {
			bytes.extend_from_slice(hash.as_bytes());
		}

		bytes.extend_from_slice(self.hash_stop.as_bytes());

		bytes
	}

	/// Deserializes a `getheaders` message from wire bytes
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);

		// The version field is the peer's negotiated protocol version, not a fixed
		// constant. Read it but don't reject -- the reference client discards it
		let _version = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read getheaders version")?;

		let count = decode_varint(&mut cursor).context("failed to decode getheaders hash count")?;

		if count > MAX_LOCATOR_HASHES as u64 {
			return Err(anyhow!(
				"getheaders hash_count {count} exceeds maximum {MAX_LOCATOR_HASHES}"
			));
		}

		// count validated <= MAX_LOCATOR_HASHES, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut locator_hashes = Vec::with_capacity(count as usize);

		for _ in 0..count {
			let mut hash_bytes = [0u8; HASH_LEN];
			cursor
				.read_exact(&mut hash_bytes)
				.context("failed to read locator hash")?;
			locator_hashes.push(Hash256::from_bytes(hash_bytes));
		}

		let mut stop_bytes = [0u8; HASH_LEN];
		cursor.read_exact(&mut stop_bytes).context("failed to read hash_stop")?;

		Ok(Self {
			locator_hashes,
			hash_stop: Hash256::from_bytes(stop_bytes),
		})
	}
}
