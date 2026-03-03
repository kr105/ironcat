// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/expect for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr};

use ironcat::network::{message_version::MessageVersion, NetworkAddress, ServiceMask};

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
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9933);
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

#[test]
fn new_reports_zero_start_height() {
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933);
	let msg = MessageVersion::new(addr, 42);
	assert_eq!(msg.start_height, 0, "should report honest 0 height");
}

#[test]
fn new_uses_cargo_version_in_user_agent() {
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933);
	let msg = MessageVersion::new(addr, 42);
	let expected = format!("/Ironcat:{}/", env!("CARGO_PKG_VERSION"));
	assert_eq!(msg.user_agent, expected);
}
