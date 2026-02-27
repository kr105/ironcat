// SPDX-License-Identifier: Apache-2.0

use std::io::{Cursor, Read};

use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};

use super::{decode_varint, write_varint, NetworkAddress};
use crate::utils::unix_now;

/// An entry in an addr message, pairing a network address with its timestamp
#[derive(Debug)]
pub struct AddrEntry {
	/// When this address was last seen active
	pub timestamp: u32,
	/// The network address
	pub address: NetworkAddress,
}

/// Represents an addr message in the Catcoin protocol
pub struct MessageAddr {
	addr_list: Vec<AddrEntry>,
}

impl MessageAddr {
	/// Creates a new addr message from a list of network addresses, using the current timestamp
	pub fn new(nodes_list: Vec<NetworkAddress>) -> Self {
		// Catcoin protocol uses u32 timestamps, valid until 2106
		#[allow(clippy::cast_possible_truncation)]
		let timestamp = unix_now() as u32;

		let addr_list = nodes_list
			.into_iter()
			.map(|address| AddrEntry { timestamp, address })
			.collect();

		Self { addr_list }
	}

	/// Creates an addr message from pre-built entries, preserving their original timestamps
	///
	/// Used for relaying: we forward the timestamp the originator set, not our own
	pub const fn from_entries(entries: Vec<AddrEntry>) -> Self {
		Self { addr_list: entries }
	}

	/// Decodes an addr message from wire bytes
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);
		let count = decode_varint(&mut cursor).context("failed to decode addr count")?;

		if count > 1000 {
			return Err(anyhow!("addr message claims {count} entries, max is 1000"));
		}

		// count is validated <= 1000, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut addr_list = Vec::with_capacity(count as usize);

		for _ in 0..count {
			let timestamp = cursor
				.read_u32::<LittleEndian>()
				.context("failed to read addr timestamp")?;

			let mut buffer = [0u8; 26];
			cursor
				.read_exact(&mut buffer)
				.context("failed to read addr network address bytes")?;

			let address = NetworkAddress::from_bytes(&buffer).context("failed to parse addr network address")?;

			addr_list.push(AddrEntry { timestamp, address });
		}

		Ok(Self { addr_list })
	}

	/// Returns the entries in this addr message
	pub fn entries(&self) -> &[AddrEntry] {
		&self.addr_list
	}

	/// Converts the addr message to bytes for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		// varint (max 9 bytes) + 30 bytes per entry (4 timestamp + 26 network address)
		// addr_list.len() is validated <= 1000, multiplication can't overflow
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 9 + self.addr_list.len() * 30;
		let mut bytes = Vec::with_capacity(capacity);

		// addr_list.len() bounded by 1000, fits in u64
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.addr_list.len() as u64);

		for entry in &self.addr_list {
			bytes.extend_from_slice(&entry.timestamp.to_le_bytes());
			bytes.extend_from_slice(&entry.address.services.bits().to_le_bytes());
			bytes.extend_from_slice(&entry.address.address_to_network_bytes());
			bytes.extend_from_slice(&entry.address.port.to_be_bytes());
		}

		bytes
	}
}
