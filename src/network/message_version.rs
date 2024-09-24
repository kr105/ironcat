// SPDX-License-Identifier: Apache-2.0

use super::{decode_varstr, encode_varstr, NetworkAddress, ServiceMask};
use anyhow::{anyhow, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use rand::RngCore;
use std::{
	io::{Cursor, Read},
	time::{SystemTime, UNIX_EPOCH},
};

const USER_AGENT: &str = "/Ironcat:0.0.1/";
const PROTOCOL_VERSION: u32 = 70003;

/// Represents a version message in the Catcoin protocol
#[derive(Debug)]
pub struct MessageVersion {
	/// Identifies protocol version being used by the node
	pub version: u32,
	/// bitfield of features to be enabled for this connection
	pub services: ServiceMask,
	/// standard UNIX timestamp in seconds
	pub timestamp: i64,
	/// The network address of the node receiving this message
	pub addr_recv: NetworkAddress,
	// Field can be ignored. This used to be the network address of the node emitting this message, but most P2P implementations send 26 dummy bytes
	//pub addr_from: Address,
	/// Node random nonce, randomly generated every time a version packet is sent. This nonce is used to detect connections to self
	pub nonce: u64,
	/// User Agent (0x00 if string is 0 bytes long)
	pub user_agent: String,
	/// The last block received by the emitting node
	pub start_height: i32,
	/// Whether the remote peer should announce relayed transactions or not, see BIP 0037
	pub relay: bool,
}

impl MessageVersion {
	pub fn new(addr_recv: NetworkAddress) -> Self {
		let mut services = ServiceMask::empty();
		services.set(ServiceMask::NODE_NETWORK, true);

		let timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("Time went backwards")
			.as_secs() as i64;

		MessageVersion {
			version: PROTOCOL_VERSION,
			services,
			timestamp,
			addr_recv,
			nonce: rand::thread_rng().next_u64(),
			user_agent: USER_AGENT.to_string(),
			start_height: 300000,
			relay: true,
		}
	}

	/// Converts the MessageVersion to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(85 + self.user_agent.len()); // Pre-allocate with estimated size
		bytes.extend_from_slice(&self.version.to_le_bytes());
		bytes.extend_from_slice(&self.services.bits().to_le_bytes());
		bytes.extend_from_slice(&self.timestamp.to_le_bytes());
		bytes.extend(&self.addr_recv.to_bytes());
		bytes.extend_from_slice(&[0u8; 26]); // Placeholder for addr_from
		bytes.extend_from_slice(&self.nonce.to_le_bytes());
		bytes.extend_from_slice(&encode_varstr(&self.user_agent));
		bytes.extend_from_slice(&self.start_height.to_le_bytes());
		bytes.push(self.relay as u8);
		bytes
	}

	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		if bytes.len() < 85 {
			return Err(anyhow!("Insufficient bytes for MessageVersion"));
		}

		let mut cursor = Cursor::new(bytes);

		let version = cursor.read_u32::<LittleEndian>()?;
		let services = ServiceMask::from_bits(cursor.read_u64::<LittleEndian>()?).unwrap();
		let timestamp = cursor.read_i64::<LittleEndian>()?;

		let mut address = [0u8; 26];
		cursor.read_exact(&mut address)?;

		let addr_recv = NetworkAddress::from_bytes(&address)?;

		// Skip addr_from (26 bytes)
		cursor.read_exact(&mut address)?;

		let nonce = cursor.read_u64::<LittleEndian>()?;

		// Read user_agent (varstr)
		let user_agent = decode_varstr(&mut cursor)?;

		let start_height = cursor.read_i32::<LittleEndian>()?;

		// Latest "stable" Catcoin client has version 70003
		// and does not include this field on this message
		let relay = if version > 70003 { cursor.read_u8()? != 0 } else { false };

		Ok(MessageVersion {
			version,
			services,
			timestamp,
			addr_recv,
			nonce,
			user_agent,
			start_height,
			relay,
		})
	}
}
