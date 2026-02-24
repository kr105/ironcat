// SPDX-License-Identifier: Apache-2.0

use std::time::{SystemTime, UNIX_EPOCH};

use super::{encode_varint, NetworkAddress};

pub struct MessageAddr {
	addr_list: Vec<NetworkAddress>,
}

impl MessageAddr {
	pub const fn new(nodes_list: Vec<NetworkAddress>) -> Self {
		Self { addr_list: nodes_list }
	}

	/// Converts the `MessageAddr` to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		// Just in case someone calls this with an empty list
		if self.addr_list.is_empty() {
			return Vec::new();
		}

		let count = self.addr_list.len() as u64;

		// SystemTime::now().duration_since(UNIX_EPOCH) only fails if system clock
		// is before 1970, which is not a realistic scenario
		#[allow(clippy::expect_used)]
		let timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("Time went backwards")
			.as_secs();

		// Catcoin protocol uses u32 timestamps, valid until 2106
		#[allow(clippy::cast_possible_truncation)]
		let timestamp = timestamp as u32;

		let mut bytes = Vec::with_capacity(85); // Pre-allocate with estimated size
		bytes.extend_from_slice(&encode_varint(count));

		for addr in &self.addr_list {
			bytes.extend_from_slice(&timestamp.to_le_bytes());
			bytes.extend_from_slice(&addr.services.bits().to_le_bytes());
			bytes.extend_from_slice(&addr.address_to_network_bytes());
			bytes.extend_from_slice(&addr.port.to_be_bytes());
		}

		bytes
	}
}
