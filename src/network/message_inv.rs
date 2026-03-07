// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use tracing::debug;

use super::{decode_varint, write_varint};
use crate::types::hash::{Hash256, HASH_LEN};

/// Maximum number of inventory items in a single message
const MAX_INV_SIZE: usize = 50_000;

/// Witness flag bit in inv type (1 << 30)
const MSG_WITNESS_FLAG: u32 = 1 << 30;

/// MWEB flag bit in inv type (1 << 29)
const MSG_MWEB_FLAG: u32 = 1 << 29;

/// Inventory item type, identifying what kind of data the hash refers to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum InvType {
	/// Transaction
	Tx = 1,
	/// Block
	Block = 2,
	/// Filtered block (BIP37)
	FilteredBlock = 3,
	/// Compact block (BIP152)
	CompactBlock = 4,
}

impl InvType {
	/// Parses an inventory type from a raw u32, stripping witness and MWEB flags
	///
	/// Returns `None` for unknown types (caller should skip, not error)
	const fn from_u32(n: u32) -> Option<Self> {
		// Strip witness (1<<30) and MWEB (1<<29) flags before matching
		let stripped = n & !(MSG_WITNESS_FLAG | MSG_MWEB_FLAG);
		match stripped {
			1 => Some(Self::Tx),
			2 => Some(Self::Block),
			3 => Some(Self::FilteredBlock),
			4 => Some(Self::CompactBlock),
			_ => None,
		}
	}
}

/// A single inventory item (type + hash), 36 bytes on wire
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvItem {
	/// What kind of data this refers to
	pub inv_type: InvType,
	/// Hash identifying the data
	pub hash: Hash256,
}

impl InvItem {
	/// Serializes the item to 36 bytes (4 type + 32 hash)
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(36);
		bytes.extend_from_slice(&(self.inv_type as u32).to_le_bytes());
		bytes.extend_from_slice(self.hash.as_bytes());
		bytes
	}

	/// Deserializes an item from a cursor, returning `None` for unknown types
	fn from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Option<Self>> {
		let raw_type = cursor.read_u32::<LittleEndian>().context("failed to read inv type")?;

		let mut hash_bytes = [0u8; HASH_LEN];
		cursor.read_exact(&mut hash_bytes).context("failed to read inv hash")?;

		let Some(inv_type) = InvType::from_u32(raw_type) else {
			debug!(raw_type, "skipping unknown inv type");
			return Ok(None);
		};

		Ok(Some(Self {
			inv_type,
			hash: Hash256::from_bytes(hash_bytes),
		}))
	}
}

/// An inventory message payload, shared by inv, getdata, and notfound commands
///
/// All three commands use the same wire format: a count followed by that many
/// `InvItem`s. The command name determines the semantic meaning
pub struct MessageInv {
	items: Vec<InvItem>,
}

impl MessageInv {
	/// Creates a new inventory message from a list of items
	pub const fn new(items: Vec<InvItem>) -> Self {
		Self { items }
	}

	/// Returns the items in this message
	pub fn items(&self) -> &[InvItem] {
		&self.items
	}

	/// Consumes the message and returns the items
	pub fn into_items(self) -> Vec<InvItem> {
		self.items
	}

	/// Decodes an inventory message from wire bytes
	///
	/// Unknown inventory types are silently skipped (not an error), matching
	/// how ironcat handles unknown service flags with `from_bits_truncate`
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);
		let count = decode_varint(&mut cursor).context("failed to decode inv count")?;

		if count > MAX_INV_SIZE as u64 {
			return Err(anyhow!("inv message claims {count} items, max is {MAX_INV_SIZE}"));
		}

		// count validated <= MAX_INV_SIZE, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut items = Vec::with_capacity(count as usize);

		for _ in 0..count {
			if let Some(item) = InvItem::from_cursor(&mut cursor)? {
				items.push(item);
			}
		}

		Ok(Self { items })
	}

	/// Serializes the inventory message to wire bytes
	pub fn to_bytes(&self) -> Vec<u8> {
		// varint (max 9) + 36 bytes per item
		// items.len() is bounded by MAX_INV_SIZE (50K), multiplication can't overflow
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 9 + self.items.len() * 36;
		let mut bytes = Vec::with_capacity(capacity);

		// items.len() bounded by MAX_INV_SIZE, fits in u64
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.items.len() as u64);

		for item in &self.items {
			bytes.extend_from_slice(&item.to_bytes());
		}

		bytes
	}
}
