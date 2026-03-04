// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};

use super::hash::{double_sha256, Hash256, HASH_LEN};

/// Size of a serialized block header in bytes
pub const HEADER_SIZE: usize = 80;

/// An 80-byte block header
///
/// The `block_hash()` method returns the identity hash (double SHA-256 of the
/// serialized header). Note that Catcoin uses Scrypt for proof-of-work, so the
/// proof-of-work hash is different from the identity hash
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHeader {
	/// Block version (currently 1 or 2 in Catcoin)
	pub version: i32,
	/// Hash of the previous block header
	pub prev_hash: Hash256,
	/// Root of the Merkle tree of transactions
	pub merkle_root: Hash256,
	/// Block creation timestamp (Unix seconds)
	pub timestamp: u32,
	/// Encoded difficulty target
	pub bits: u32,
	/// Nonce varied by miners to find a valid proof-of-work
	pub nonce: u32,
}

impl BlockHeader {
	/// Serializes the header to exactly 80 bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(HEADER_SIZE);
		bytes.extend_from_slice(&self.version.to_le_bytes());
		bytes.extend_from_slice(self.prev_hash.as_bytes());
		bytes.extend_from_slice(self.merkle_root.as_bytes());
		bytes.extend_from_slice(&self.timestamp.to_le_bytes());
		bytes.extend_from_slice(&self.bits.to_le_bytes());
		bytes.extend_from_slice(&self.nonce.to_le_bytes());
		bytes
	}

	/// Deserializes a header from a byte slice (must be >= 80 bytes)
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() < HEADER_SIZE {
			return Err(anyhow!(
				"block header too short: {} bytes, need {}",
				bytes.len(),
				HEADER_SIZE
			));
		}

		let mut cursor = Cursor::new(bytes);
		Self::from_cursor(&mut cursor)
	}

	/// Deserializes a header from a cursor, advancing it by 80 bytes
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let version = cursor
			.read_i32::<LittleEndian>()
			.context("failed to read block header version")?;

		let mut prev_bytes = [0u8; HASH_LEN];
		cursor
			.read_exact(&mut prev_bytes)
			.context("failed to read block header prev_hash")?;
		let prev_hash = Hash256::from_bytes(prev_bytes);

		let mut merkle_bytes = [0u8; HASH_LEN];
		cursor
			.read_exact(&mut merkle_bytes)
			.context("failed to read block header merkle_root")?;
		let merkle_root = Hash256::from_bytes(merkle_bytes);

		let timestamp = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read block header timestamp")?;
		let bits = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read block header bits")?;
		let nonce = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read block header nonce")?;

		Ok(Self {
			version,
			prev_hash,
			merkle_root,
			timestamp,
			bits,
			nonce,
		})
	}

	/// Computes the identity hash of this block header (double SHA-256)
	///
	/// This is the hash used to reference blocks in the protocol. It is NOT
	/// the proof-of-work hash (Catcoin uses Scrypt for proof-of-work)
	pub fn block_hash(&self) -> Hash256 {
		double_sha256(&self.to_bytes())
	}
}
