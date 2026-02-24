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

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
	use super::*;
	use std::net::{IpAddr, Ipv4Addr};

	#[test]
	fn roundtrip_single_addr() {
		let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9933);
		let msg = MessageAddr::new(vec![addr]);
		let bytes = msg.to_bytes();
		let decoded = MessageAddr::from_bytes(&bytes).unwrap();
		assert_eq!(decoded.entries().len(), 1);
		assert_eq!(decoded.entries()[0].address.port, 9933);
	}

	#[test]
	fn roundtrip_multiple_addrs() {
		let addrs: Vec<NetworkAddress> = (1..=5)
			.map(|i| NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)), 9933))
			.collect();
		let msg = MessageAddr::new(addrs);
		let bytes = msg.to_bytes();
		let decoded = MessageAddr::from_bytes(&bytes).unwrap();
		assert_eq!(decoded.entries().len(), 5);
	}

	#[test]
	fn from_bytes_rejects_count_over_1000() {
		let mut bytes = vec![0xFD];
		bytes.extend_from_slice(&1001u16.to_le_bytes());
		let result = MessageAddr::from_bytes(&bytes);
		assert!(result.is_err());
	}

	#[test]
	fn empty_addr_list_produces_varint_zero() {
		let msg = MessageAddr::new(Vec::new());
		assert_eq!(msg.to_bytes(), vec![0x00]);
	}

	#[test]
	fn addresses_preserve_ports() {
		let addrs = vec![
			NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933),
			NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 8080),
		];
		let msg = MessageAddr::new(addrs);
		let bytes = msg.to_bytes();
		let decoded = MessageAddr::from_bytes(&bytes).unwrap();
		assert_eq!(decoded.entries()[0].address.port, 9933);
		assert_eq!(decoded.entries()[1].address.port, 8080);
	}
}
