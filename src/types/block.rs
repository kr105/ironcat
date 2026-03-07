// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};

use super::hash::{double_sha256, Hash256, HASH_LEN};
use super::transaction::Transaction;
use crate::network::decode_varint;

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

/// Maximum number of transactions in a block
///
/// A generous upper bound to prevent OOM from malicious data. Real blocks
/// are limited by the 1MB block size, which fits far fewer transactions
const MAX_BLOCK_TXS: u64 = 100_000;

/// A full block: header plus transactions
#[derive(Debug, Clone)]
pub struct Block {
	/// The 80-byte block header
	pub header: BlockHeader,
	/// Transactions included in this block
	pub transactions: Vec<Transaction>,
}

impl Block {
	/// Deserializes a block from a byte slice
	///
	/// Parses the 80-byte header, varint tx count, and each transaction
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);
		let header = BlockHeader::from_cursor(&mut cursor).context("failed to parse block header")?;

		let tx_count = decode_varint(&mut cursor).context("failed to read block tx count")?;
		if tx_count > MAX_BLOCK_TXS {
			return Err(anyhow!("block tx count {tx_count} exceeds maximum {MAX_BLOCK_TXS}"));
		}

		// tx_count validated <= MAX_BLOCK_TXS (100K), safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut transactions = Vec::with_capacity(tx_count as usize);
		for i in 0..tx_count {
			let tx =
				Transaction::from_cursor(&mut cursor).with_context(|| format!("failed to parse transaction {i}"))?;
			transactions.push(tx);
		}

		Ok(Self { header, transactions })
	}

	/// Deserializes a block and validates that the merkle root matches
	///
	/// Returns an error if the block has no transactions or if the computed
	/// merkle root does not match the header's `merkle_root` field
	pub fn from_bytes_and_validate(bytes: &[u8]) -> Result<Self> {
		let block = Self::from_bytes(bytes)?;

		if block.transactions.is_empty() {
			return Err(anyhow!("block has no transactions"));
		}

		let txids: Vec<Hash256> = block.transactions.iter().map(Transaction::txid).collect();

		let (computed, mutated) = compute_merkle_root(&txids);

		// CVE-2012-2459: reject blocks with duplicate subtrees at any tree level.
		// Bitcoin's merkle tree duplicates the last hash when count is odd, so
		// different tx lists can produce the same root. The mutation flag is set
		// when any adjacent pair being hashed together is identical at any level
		if mutated {
			return Err(anyhow!("block contains duplicate transaction (CVE-2012-2459)"));
		}

		if computed != block.header.merkle_root {
			return Err(anyhow!(
				"merkle root mismatch: header has {}, computed {}",
				block.header.merkle_root,
				computed
			));
		}

		Ok(block)
	}
}

/// Computes a Bitcoin-style merkle root from a list of transaction hashes
///
/// At each level, if the number of hashes is odd the last hash is duplicated.
/// Adjacent pairs are concatenated and double-SHA256'd until one root remains.
///
/// Returns `(root, mutated)` where `mutated` is true if any adjacent pair
/// being hashed together was identical at any tree level (CVE-2012-2459).
/// This follows Bitcoin Core's `ComputeMerkleRoot` detection strategy
///
/// # Panics
///
/// Panics if `txids` is empty (caller must verify non-empty input)
pub fn compute_merkle_root(txids: &[Hash256]) -> (Hash256, bool) {
	assert!(!txids.is_empty(), "compute_merkle_root called with empty txids");

	let mut mutated = false;
	let mut level: Vec<Hash256> = txids.to_vec();

	while level.len() > 1 {
		// CVE-2012-2459: check all complete pairs for equality BEFORE
		// the odd-count duplication step. If any pair being hashed together
		// is identical, a shorter tx list can produce the same root.
		// This matches Bitcoin Core's ComputeMerkleRoot detection
		#[allow(clippy::indexing_slicing)] // chunks_exact(2) guarantees 2 elements
		for pair in level.chunks_exact(2) {
			if pair[0] == pair[1] {
				mutated = true;
			}
		}

		// Duplicate last if odd count
		if !level.len().is_multiple_of(2) {
			if let Some(&last) = level.last() {
				level.push(last);
			}
		}

		// chunks_exact(2) guarantees each slice has exactly 2 elements
		#[allow(clippy::indexing_slicing)]
		let next_level: Vec<Hash256> = level
			.chunks_exact(2)
			.map(|pair| {
				let mut combined = [0u8; 64];
				combined[..32].copy_from_slice(pair[0].as_bytes());
				combined[32..].copy_from_slice(pair[1].as_bytes());
				double_sha256(&combined)
			})
			.collect();
		level = next_level;
	}

	// assert! at the top guarantees at least one element entered the loop,
	// and each iteration halves the count, converging to exactly one
	#[allow(clippy::indexing_slicing)] // level always has exactly one element here
	(level[0], mutated)
}
