// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::{
	io::Cursor,
	net::{IpAddr, Ipv4Addr},
};

use ironcat::network::{
	decode_varstr, write_varstr, Message, MessageParseError, NetworkAddress, NetworkQueue, MAX_MESSAGE_SIZE,
};

/// Helper to build a varstr for test data (mirrors the cfg(test) encode_varstr in src)
fn encode_varstr(s: &str) -> Vec<u8> {
	let mut buf = Vec::new();
	write_varstr(&mut buf, s);
	buf
}

#[test]
fn network_address_port_roundtrip() {
	// Encode then decode should give the same port
	let port: u16 = 9933;
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);

	let bytes = addr.to_bytes();
	let decoded = NetworkAddress::from_bytes(&bytes).expect("should decode");

	assert_eq!(decoded.port, port);
}

#[test]
fn decode_varstr_rejects_oversized_length() {
	// Craft a varstr with length = 0xFFFF (65535), way over any sane limit
	// but only 4 bytes of actual data after it
	let mut data = vec![0xFD, 0xFF, 0xFF]; // varint = 65535
	data.extend_from_slice(&[0x41; 4]); // only 4 bytes of "AAAA"

	let mut cursor = Cursor::new(data.as_slice());
	let result = decode_varstr(&mut cursor);

	assert!(result.is_err(), "should reject varstr with length > MAX_VARSTR_LENGTH");
}

#[test]
fn decode_varstr_accepts_valid_string() {
	let encoded = encode_varstr("hello");
	let mut cursor = Cursor::new(encoded.as_slice());
	let result = decode_varstr(&mut cursor).unwrap();

	assert_eq!(result, "hello");
}

#[test]
fn network_address_port_is_big_endian_on_wire() {
	// Port 0x1F90 (8080) should appear as [0x1F, 0x90] in the last 2 bytes
	let port: u16 = 8080;
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port);

	let bytes = addr.to_bytes();

	// The wire format is: 8 bytes services + 16 bytes IP + 2 bytes port
	let port_bytes = &bytes[24..26];
	assert_eq!(port_bytes, &port.to_be_bytes(), "port must be big-endian on the wire");
}

#[test]
fn from_bytes_returns_consumed_length() {
	let msg = Message::new("ping", &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
	let bytes = msg.to_bytes();
	let mut extended = bytes.clone();
	extended.extend_from_slice(&[0xFF; 50]);

	let (parsed, consumed) = Message::from_bytes(&extended).unwrap();
	assert_eq!(consumed, bytes.len());
	assert_eq!(parsed.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn from_bytes_rejects_bad_magic_immediately() {
	let mut bytes = vec![0x00, 0x00, 0x00, 0x00];
	bytes.extend_from_slice(&[0u8; 20]);
	let err = Message::from_bytes(&bytes).unwrap_err();
	assert!(matches!(err, MessageParseError::Corrupt(_)));
}

#[test]
fn from_bytes_returns_incomplete_for_short_buffer() {
	let bytes = vec![0xFC, 0xC1, 0xB7, 0xDC];
	let err = Message::from_bytes(&bytes).unwrap_err();
	assert!(matches!(err, MessageParseError::Incomplete));
}

#[test]
fn from_bytes_rejects_oversized_declared_length() {
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&[0xFC, 0xC1, 0xB7, 0xDC]);
	bytes.extend_from_slice(&[0u8; 12]);
	bytes.extend_from_slice(&((MAX_MESSAGE_SIZE as u32) + 1).to_le_bytes());
	bytes.extend_from_slice(&[0u8; 4]);
	let err = Message::from_bytes(&bytes).unwrap_err();
	assert!(matches!(err, MessageParseError::Corrupt(_)));
}

#[test]
fn process_incoming_data_detects_corrupt_magic() {
	let mut queue = NetworkQueue::new();
	let bad_data = vec![
		0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
	];
	let result = queue.process_incoming_data(&bad_data);
	assert!(result.is_err());
}
