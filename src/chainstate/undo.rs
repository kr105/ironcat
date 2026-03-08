// SPDX-License-Identifier: Apache-2.0

//! Undo data for block disconnection
//!
//! When connecting a block, the spent coins are recorded so the UTXO set
//! can be restored if the block needs to be disconnected during a reorg

use std::io::Cursor;

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};

use super::coin::Coin;

/// Undo data for a single transaction
///
/// Stores the coins that were spent by this transaction's inputs,
/// in the same order as the inputs. Coinbase transactions have no
/// undo data since they don't spend any coins
#[derive(Debug, Clone)]
pub struct TxUndo {
	/// The coins spent by each input, in input order
	pub spent_outputs: Vec<Coin>,
}

impl TxUndo {
	/// Serializes the transaction undo data to bytes
	///
	/// Format: `count(4 LE) | for each coin: coin_len(4 LE) | coin_bytes`
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::new();

		// spent_outputs.len() fits in u32 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		let count = self.spent_outputs.len() as u32;
		bytes.extend_from_slice(&count.to_le_bytes());

		for coin in &self.spent_outputs {
			let coin_bytes = coin.to_bytes();

			// coin_bytes.len() fits in u32 (bounded by script size)
			#[allow(clippy::cast_possible_truncation)]
			let coin_len = coin_bytes.len() as u32;
			bytes.extend_from_slice(&coin_len.to_le_bytes());
			bytes.extend_from_slice(&coin_bytes);
		}

		bytes
	}

	/// Deserializes transaction undo data from a byte slice
	pub fn from_bytes(data: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(data);
		Self::from_cursor(&mut cursor)
	}

	/// Deserializes transaction undo data from a cursor
	pub fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
		let count = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read tx undo coin count")?;

		let mut spent_outputs = Vec::with_capacity(count as usize);

		for _ in 0..count {
			let coin_len = cursor
				.read_u32::<LittleEndian>()
				.context("failed to read tx undo coin length")?;

			let pos = cursor.position();
			let inner = cursor.get_ref();

			// pos and coin_len are both u32-range values; on 64-bit their sum fits usize
			#[allow(clippy::cast_possible_truncation, clippy::arithmetic_side_effects)]
			let end = pos as usize + coin_len as usize;
			if end > inner.len() {
				anyhow::bail!("tx undo coin data extends past end of buffer");
			}

			// Bounds verified above
			#[allow(clippy::cast_possible_truncation, clippy::indexing_slicing)]
			let coin_data = &inner[pos as usize..end];
			let coin = Coin::from_bytes(coin_data).context("failed to decode coin in tx undo")?;
			spent_outputs.push(coin);

			cursor.set_position(end as u64);
		}

		Ok(Self { spent_outputs })
	}
}

/// Undo data for an entire block
///
/// Contains undo entries for every non-coinbase transaction in the block,
/// in the same order they appear in the block. The coinbase is excluded
/// because it does not spend any existing coins
#[derive(Debug, Clone)]
pub struct BlockUndo {
	/// Per-transaction undo data, one entry per non-coinbase tx
	pub tx_undos: Vec<TxUndo>,
}

impl BlockUndo {
	/// Serializes the block undo data to bytes
	///
	/// Format: `tx_count(4 LE) | for each tx_undo: undo_len(4 LE) | undo_bytes`
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::new();

		// tx_undos.len() fits in u32 on any supported platform
		#[allow(clippy::cast_possible_truncation)]
		let tx_count = self.tx_undos.len() as u32;
		bytes.extend_from_slice(&tx_count.to_le_bytes());

		for undo in &self.tx_undos {
			let undo_bytes = undo.to_bytes();

			// undo_bytes.len() fits in u32
			#[allow(clippy::cast_possible_truncation)]
			let undo_len = undo_bytes.len() as u32;
			bytes.extend_from_slice(&undo_len.to_le_bytes());
			bytes.extend_from_slice(&undo_bytes);
		}

		bytes
	}

	/// Deserializes block undo data from a byte slice
	pub fn from_bytes(data: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(data);

		let tx_count = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read block undo tx count")?;

		let mut tx_undos = Vec::with_capacity(tx_count as usize);

		for _ in 0..tx_count {
			let undo_len = cursor
				.read_u32::<LittleEndian>()
				.context("failed to read block undo entry length")?;

			let pos = cursor.position();
			let inner = cursor.get_ref();

			// pos and undo_len are both u32-range values; on 64-bit their sum fits usize
			#[allow(clippy::cast_possible_truncation, clippy::arithmetic_side_effects)]
			let end = pos as usize + undo_len as usize;
			if end > inner.len() {
				anyhow::bail!("block undo entry extends past end of buffer");
			}

			// Bounds verified above
			#[allow(clippy::cast_possible_truncation, clippy::indexing_slicing)]
			let undo_data = &inner[pos as usize..end];
			let undo = TxUndo::from_bytes(undo_data).context("failed to decode tx undo in block undo")?;
			tx_undos.push(undo);

			cursor.set_position(end as u64);
		}

		Ok(Self { tx_undos })
	}
}
