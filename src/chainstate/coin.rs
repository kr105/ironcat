// SPDX-License-Identifier: Apache-2.0

//! UTXO coin type and serialization

use std::io::{Cursor, Read};

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};

use crate::types::transaction::TxOut;

/// A spendable coin in the UTXO set
///
/// Wraps a transaction output with the metadata needed for consensus
/// validation: the block height where it was created and whether it
/// came from a coinbase transaction (for maturity checks)
#[derive(Debug, Clone)]
pub struct Coin {
	/// The transaction output (value + scriptPubKey)
	pub tx_out: TxOut,
	/// Block height where this coin was created
	pub height: u32,
	/// Whether this coin comes from a coinbase transaction
	pub is_coinbase: bool,
}

impl Coin {
	/// Serializes the coin to bytes
	///
	/// Format: `height(4 LE) | is_coinbase(1) | value(8 LE) | script_len(4 LE) | script`
	pub fn to_bytes(&self) -> Vec<u8> {
		// 4 + 1 + 8 + 4 + script
		#[allow(clippy::arithmetic_side_effects)] // all constants, no overflow
		let capacity = 17 + self.tx_out.script_pubkey.len();
		let mut bytes = Vec::with_capacity(capacity);
		bytes.extend_from_slice(&self.height.to_le_bytes());
		bytes.push(u8::from(self.is_coinbase));
		bytes.extend_from_slice(&self.tx_out.value.to_le_bytes());

		// script_pubkey.len() fits in u32 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		let script_len = self.tx_out.script_pubkey.len() as u32;
		bytes.extend_from_slice(&script_len.to_le_bytes());
		bytes.extend_from_slice(&self.tx_out.script_pubkey);
		bytes
	}

	/// Deserializes a coin from a byte slice
	///
	/// Returns an error if the data is too short or malformed
	pub fn from_bytes(data: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(data);
		Self::from_cursor(&mut cursor)
	}

	/// Deserializes a coin from a cursor
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let height = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read coin height")?;

		let mut flag = [0u8; 1];
		cursor
			.read_exact(&mut flag)
			.context("failed to read coin is_coinbase flag")?;
		let is_coinbase = flag[0] != 0;

		let value = cursor.read_i64::<LittleEndian>().context("failed to read coin value")?;

		let script_len = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read coin script length")?;

		let mut script_pubkey = vec![0u8; script_len as usize];
		cursor
			.read_exact(&mut script_pubkey)
			.context("failed to read coin script bytes")?;

		Ok(Self {
			tx_out: TxOut { value, script_pubkey },
			height,
			is_coinbase,
		})
	}
}
