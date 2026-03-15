// SPDX-License-Identifier: Apache-2.0

use super::{NetworkAddress, ServiceMask, decode_varstr, write_varstr};
use crate::utils::unix_now;
use anyhow::{Context, Result, anyhow};
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};

const PROTOCOL_VERSION: u32 = 70012;

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
	/// Creates a new version message for the given receiving address, nonce, and chain height
	pub fn new(addr_recv: NetworkAddress, nonce: u64, start_height: u32) -> Self {
		let mut services = ServiceMask::empty();
		services.set(ServiceMask::NODE_NETWORK, true);

		// Protocol uses i64 for timestamp; u64 seconds won't wrap for ~584 billion years
		#[allow(clippy::cast_possible_wrap)]
		let timestamp = unix_now() as i64;

		// start_height is i32 on the wire; u32 tip won't exceed i32::MAX in practice
		#[allow(clippy::cast_possible_wrap)]
		let start_height = start_height as i32;

		Self {
			version: PROTOCOL_VERSION,
			services,
			timestamp,
			addr_recv,
			nonce,
			user_agent: format!("/Ironcat:{}/", env!("CARGO_PKG_VERSION")),
			start_height,
			relay: true,
		}
	}

	/// Converts the `MessageVersion` to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		// user_agent is a short string, can't overflow usize
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 86 + self.user_agent.len();

		let mut bytes = Vec::with_capacity(capacity); // Pre-allocate with estimated size
		bytes.extend_from_slice(&self.version.to_le_bytes());
		bytes.extend_from_slice(&self.services.bits().to_le_bytes());
		bytes.extend_from_slice(&self.timestamp.to_le_bytes());
		bytes.extend_from_slice(&self.addr_recv.to_bytes());
		bytes.extend_from_slice(&[0u8; 26]); // Placeholder for addr_from
		bytes.extend_from_slice(&self.nonce.to_le_bytes());
		write_varstr(&mut bytes, &self.user_agent);
		bytes.extend_from_slice(&self.start_height.to_le_bytes());
		bytes.push(u8::from(self.relay));
		bytes
	}

	/// Decodes a version message from wire bytes
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		// Minimum: version(4) + services(8) + timestamp(8) + addr_recv(26) + addr_from(26) +
		//          nonce(8) + user_agent_varint(1) + start_height(4) + relay(1) = 86
		if bytes.len() < 86 {
			return Err(anyhow!("Insufficient bytes for MessageVersion"));
		}

		let mut cursor = Cursor::new(bytes);

		let version = cursor
			.read_u32::<LittleEndian>()
			.context("failed to read version field")?;
		let services = ServiceMask::from_bits_truncate(
			cursor
				.read_u64::<LittleEndian>()
				.context("failed to read services field")?,
		);
		let timestamp = cursor
			.read_i64::<LittleEndian>()
			.context("failed to read timestamp field")?;

		let mut address = [0u8; 26];
		cursor.read_exact(&mut address).context("failed to read addr_recv")?;

		let addr_recv = NetworkAddress::from_bytes(&address).context("failed to parse addr_recv")?;

		// Skip addr_from (26 bytes)
		cursor.read_exact(&mut address).context("failed to read addr_from")?;

		let nonce = cursor.read_u64::<LittleEndian>().context("failed to read nonce")?;

		// Read user_agent (varstr)
		let user_agent = decode_varstr(&mut cursor).context("failed to read user_agent")?;

		let start_height = cursor
			.read_i32::<LittleEndian>()
			.context("failed to read start_height")?;

		let relay = cursor.read_u8().context("failed to read relay field")? != 0;

		Ok(Self {
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
