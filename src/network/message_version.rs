// SPDX-License-Identifier: Apache-2.0

use super::{decode_varstr, write_varstr, NetworkAddress, ServiceMask};
use crate::utils::unix_now;
use anyhow::{anyhow, Context, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};

const USER_AGENT: &str = "/Ironcat:0.0.4/";
const PROTOCOL_VERSION: u32 = 70003;

/// Placeholder start height until actual chain state is available
const DEFAULT_START_HEIGHT: i32 = 300_000;

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
	/// Creates a new version message for the given receiving address and nonce
	pub fn new(addr_recv: NetworkAddress, nonce: u64) -> Self {
		let mut services = ServiceMask::empty();
		services.set(ServiceMask::NODE_NETWORK_LIMITED, true);

		// Protocol uses i64 for timestamp; u64 seconds won't wrap for ~584 billion years
		#[allow(clippy::cast_possible_wrap)]
		let timestamp = unix_now() as i64;

		Self {
			version: PROTOCOL_VERSION,
			services,
			timestamp,
			addr_recv,
			nonce,
			user_agent: USER_AGENT.to_string(),
			start_height: DEFAULT_START_HEIGHT,
			relay: true,
		}
	}

	/// Converts the `MessageVersion` to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		// user_agent is a short string, can't overflow usize
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 85 + self.user_agent.len();

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
		if bytes.len() < 85 {
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

#[cfg(test)]
// Tests use unwrap for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;
	use std::net::{IpAddr, Ipv4Addr};

	/// Build a minimal valid version message payload with the given service bits
	fn build_version_payload(services_bits: u64) -> Vec<u8> {
		let mut bytes = Vec::new();

		// version: 70003 (u32 LE)
		bytes.extend_from_slice(&70003u32.to_le_bytes());
		// services (u64 LE)
		bytes.extend_from_slice(&services_bits.to_le_bytes());
		// timestamp (i64 LE)
		bytes.extend_from_slice(&1_000_000i64.to_le_bytes());
		// addr_recv: 26 bytes (services u64 LE + 16 bytes IPv6 + 2 bytes port BE)
		let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9933);
		bytes.extend_from_slice(&addr.to_bytes());
		// addr_from: 26 zero bytes (ignored)
		bytes.extend_from_slice(&[0u8; 26]);
		// nonce (u64 LE)
		bytes.extend_from_slice(&42u64.to_le_bytes());
		// user_agent: empty varstr (1 byte length = 0)
		bytes.push(0x00);
		// start_height (i32 LE)
		bytes.extend_from_slice(&300_000i32.to_le_bytes());
		// relay (BIP 37)
		bytes.push(0x01);

		bytes
	}

	#[test]
	fn from_bytes_with_unknown_service_bits_does_not_panic() {
		// Bit 1<<15 is not defined in ServiceMask, simulating a remote node
		// advertising a service we don't know about
		let unknown_bit: u64 = 1 << 15;
		let services_bits = ServiceMask::NODE_NETWORK.bits() | unknown_bit;

		let payload = build_version_payload(services_bits);
		let result = MessageVersion::from_bytes(&payload);

		assert!(result.is_ok(), "from_bytes should not fail on unknown service bits");

		let msg = result.unwrap();
		// The known bit should survive truncation
		assert!(msg.services.contains(ServiceMask::NODE_NETWORK));
		// The unknown bit should be silently dropped
		assert_eq!(msg.services.bits() & unknown_bit, 0);
	}

	#[test]
	fn from_bytes_roundtrip() {
		let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933);
		let original = MessageVersion::new(addr, 123_456);

		let bytes = original.to_bytes();
		let decoded = MessageVersion::from_bytes(&bytes).expect("roundtrip decode should succeed");

		assert_eq!(decoded.version, original.version);
		assert_eq!(decoded.services, original.services);
		assert_eq!(decoded.nonce, original.nonce);
		assert_eq!(decoded.start_height, original.start_height);
		assert_eq!(decoded.user_agent, original.user_agent);
		assert_eq!(decoded.relay, original.relay);
	}

	#[test]
	fn from_bytes_without_relay_fails() {
		let mut payload = build_version_payload(ServiceMask::NODE_NETWORK.bits());
		payload.pop(); // Remove the relay byte
		let result = MessageVersion::from_bytes(&payload);
		assert!(result.is_err(), "missing relay field should fail parsing");
	}

	#[test]
	fn from_bytes_with_relay_false() {
		let mut payload = build_version_payload(ServiceMask::NODE_NETWORK.bits());
		// Replace relay=true with relay=false
		payload.pop();
		payload.push(0x00);
		let msg = MessageVersion::from_bytes(&payload).unwrap();
		assert!(!msg.relay);
	}
}
