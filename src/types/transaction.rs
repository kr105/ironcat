// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{Context, Result, anyhow};
use byteorder::{LittleEndian, ReadBytesExt};

use super::hash::{HASH_LEN, Hash256, double_sha256};
use crate::network::{decode_varint, write_varint};

/// Maximum size of a script in bytes (10KB)
const MAX_SCRIPT_SIZE: usize = 10_000;

/// Maximum size of a serialized transaction in bytes (1MB)
const MAX_TX_SIZE: usize = 1_000_000;

/// Maximum number of inputs or outputs in a single transaction
///
/// A 1MB block can fit at most ~24K minimal inputs (41 bytes each). Using 25K
/// as a generous upper bound that catches absurd counts without being reachable
/// by legitimate transactions
const MAX_TX_IN_OUT: u64 = 25_000;

/// A reference to a specific output of a previous transaction
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutPoint {
	/// Hash of the referenced transaction
	pub txid: Hash256,
	/// Index of the specific output in the transaction (0xFFFFFFFF for coinbase)
	pub index: u32,
}

impl OutPoint {
	/// Coinbase outpoint: zero hash with index 0xFFFFFFFF
	pub const COINBASE: Self = Self {
		txid: Hash256::ZERO,
		index: 0xFFFF_FFFF,
	};

	/// Serializes the outpoint to 36 bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(36);
		bytes.extend_from_slice(self.txid.as_bytes());
		bytes.extend_from_slice(&self.index.to_le_bytes());
		bytes
	}

	/// Deserializes an outpoint from a cursor
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let mut hash_bytes = [0u8; HASH_LEN];
		cursor
			.read_exact(&mut hash_bytes)
			.context("failed to read outpoint txid")?;
		let txid = Hash256::from_bytes(hash_bytes);
		let index = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read outpoint index")?;
		Ok(Self { txid, index })
	}
}

/// A transaction input
#[derive(Debug, Clone)]
pub struct TxIn {
	/// Reference to the output being spent
	pub prev_output: OutPoint,
	/// Unlocking script (scriptSig)
	pub script_sig: Vec<u8>,
	/// Sequence number (0xFFFFFFFF to disable locktime)
	pub sequence: u32,
}

impl TxIn {
	/// Serializes the input to bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		// 36 (outpoint) + varint + script + 4 (sequence)
		// All operands are bounded by available memory; sum can't overflow usize
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 36 + 9 + self.script_sig.len() + 4;
		let mut bytes = Vec::with_capacity(capacity);
		bytes.extend_from_slice(&self.prev_output.to_bytes());

		// script_sig.len() fits in u64 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.script_sig.len() as u64);
		bytes.extend_from_slice(&self.script_sig);
		bytes.extend_from_slice(&self.sequence.to_le_bytes());
		bytes
	}

	/// Deserializes an input from a cursor
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let prev_output = OutPoint::from_cursor(cursor).context("failed to read txin outpoint")?;

		let script_len = decode_varint(cursor).context("failed to read txin script length")?;
		if script_len > MAX_SCRIPT_SIZE as u64 {
			return Err(anyhow!(
				"txin script too large: {script_len} bytes, max {MAX_SCRIPT_SIZE}"
			));
		}

		// script_len validated <= MAX_SCRIPT_SIZE (10K), safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let script_len = script_len as usize;

		let mut script_sig = vec![0u8; script_len];
		cursor
			.read_exact(&mut script_sig)
			.context("failed to read txin script bytes")?;

		let sequence = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read txin sequence")?;

		Ok(Self {
			prev_output,
			script_sig,
			sequence,
		})
	}
}

/// A transaction output
#[derive(Debug, Clone)]
pub struct TxOut {
	/// Amount in catoshis (can be negative on wire, consensus validates later)
	pub value: i64,
	/// Locking script (scriptPubKey)
	pub script_pubkey: Vec<u8>,
}

impl TxOut {
	/// Serializes the output to bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		// 8 (value) + varint + script
		// All operands are bounded by available memory; sum can't overflow usize
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 8 + 9 + self.script_pubkey.len();
		let mut bytes = Vec::with_capacity(capacity);
		bytes.extend_from_slice(&self.value.to_le_bytes());

		// script_pubkey.len() fits in u64 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.script_pubkey.len() as u64);
		bytes.extend_from_slice(&self.script_pubkey);
		bytes
	}

	/// Deserializes an output from a cursor
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let value = cursor
			.read_i64::<LittleEndian>()
			.context("failed to read txout value")?;

		let script_len = decode_varint(cursor).context("failed to read txout script length")?;
		if script_len > MAX_SCRIPT_SIZE as u64 {
			return Err(anyhow!(
				"txout script too large: {script_len} bytes, max {MAX_SCRIPT_SIZE}"
			));
		}

		// script_len validated <= MAX_SCRIPT_SIZE (10K), safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let script_len = script_len as usize;

		let mut script_pubkey = vec![0u8; script_len];
		cursor
			.read_exact(&mut script_pubkey)
			.context("failed to read txout script bytes")?;

		Ok(Self { value, script_pubkey })
	}
}

/// A Catcoin transaction (basic format, no witness)
///
/// Catcoin has not activated segwit, so only the basic serialization is needed:
/// `version | vin_count | vin[] | vout_count | vout[] | locktime`
#[derive(Debug, Clone)]
pub struct Transaction {
	/// Transaction format version
	pub version: i32,
	/// List of inputs
	pub vin: Vec<TxIn>,
	/// List of outputs
	pub vout: Vec<TxOut>,
	/// Block height or timestamp after which this tx is valid
	pub locktime: u32,
}

impl Transaction {
	/// Serializes the transaction to bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(64);
		bytes.extend_from_slice(&self.version.to_le_bytes());

		// vin/vout lengths fit in u64 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.vin.len() as u64);
		for input in &self.vin {
			bytes.extend_from_slice(&input.to_bytes());
		}

		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.vout.len() as u64);
		for output in &self.vout {
			bytes.extend_from_slice(&output.to_bytes());
		}

		bytes.extend_from_slice(&self.locktime.to_le_bytes());
		bytes
	}

	/// Deserializes a transaction from a byte slice
	///
	/// Validates that the total size does not exceed `MAX_TX_SIZE` (1MB)
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() > MAX_TX_SIZE {
			return Err(anyhow!(
				"transaction too large: {} bytes, max {MAX_TX_SIZE}",
				bytes.len()
			));
		}
		let mut cursor = Cursor::new(bytes);
		Self::from_cursor(&mut cursor)
	}

	/// Deserializes a transaction from a cursor
	///
	/// Does not enforce `MAX_TX_SIZE` -- the caller (block parser) manages total size
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let version = cursor.read_i32::<LittleEndian>().context("failed to read tx version")?;

		let vin_count = decode_varint(cursor).context("failed to read vin count")?;
		if vin_count > MAX_TX_IN_OUT {
			return Err(anyhow!("vin count {vin_count} exceeds maximum {MAX_TX_IN_OUT}"));
		}

		// vin_count validated <= MAX_TX_IN_OUT, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut vin = Vec::with_capacity(vin_count as usize);
		for _ in 0..vin_count {
			vin.push(TxIn::from_cursor(cursor)?);
		}

		let vout_count = decode_varint(cursor).context("failed to read vout count")?;
		if vout_count > MAX_TX_IN_OUT {
			return Err(anyhow!("vout count {vout_count} exceeds maximum {MAX_TX_IN_OUT}"));
		}

		#[allow(clippy::cast_possible_truncation)]
		let mut vout = Vec::with_capacity(vout_count as usize);
		for _ in 0..vout_count {
			vout.push(TxOut::from_cursor(cursor)?);
		}

		let locktime = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read tx locktime")?;

		Ok(Self {
			version,
			vin,
			vout,
			locktime,
		})
	}

	/// Computes the transaction ID (double SHA-256 of the serialized transaction)
	pub fn txid(&self) -> Hash256 {
		double_sha256(&self.to_bytes())
	}

	/// Returns true if this is a coinbase transaction
	///
	/// A coinbase has exactly one input whose `prev_output` is the null
	/// outpoint (zero hash, index 0xFFFFFFFF)
	pub fn is_coinbase(&self) -> bool {
		self.vin.len() == 1
			&& self
				.vin
				.first()
				.is_some_and(|input| input.prev_output == OutPoint::COINBASE)
	}
}
