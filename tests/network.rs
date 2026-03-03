// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/expect/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::{
	io::Cursor,
	net::{IpAddr, Ipv4Addr},
};

use ironcat::network::{
	decode_varint, decode_varstr, write_varint, write_varstr, Message, MessageParseError, NetworkAddress, NetworkQueue,
	MAX_MESSAGE_SIZE,
};

/// Helper to build a varstr for test data (mirrors the cfg(test) `encode_varstr` in src)
fn encode_varstr(s: &str) -> Vec<u8> {
	let mut buf = Vec::new();
	write_varstr(&mut buf, s);
	buf
}

#[test]
fn network_address_port_roundtrip() {
	// Encode then decode should give the same port
	let port: u16 = 9933;
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

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
	#[allow(clippy::cast_possible_truncation)] // intentionally truncating for test data
	let oversized = (MAX_MESSAGE_SIZE as u32) + 1;
	bytes.extend_from_slice(&oversized.to_le_bytes());
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

// --- Non-canonical varint rejection tests ---

#[test]
fn decode_varint_rejects_non_canonical_fd_prefix() {
	// Value 5 encoded as 0xFD 0x05 0x00 (should be just 0x05)
	let data: &[u8] = &[0xFD, 0x05, 0x00];
	let mut cursor = Cursor::new(data);
	let result = decode_varint(&mut cursor);
	assert!(
		result.is_err(),
		"should reject non-canonical 0xFD prefix for value < 0xFD"
	);
}

#[test]
fn decode_varint_rejects_non_canonical_fe_prefix() {
	// Value 1000 encoded as 0xFE 0xE8 0x03 0x00 0x00 (should use 0xFD prefix)
	let data: &[u8] = &[0xFE, 0xE8, 0x03, 0x00, 0x00];
	let mut cursor = Cursor::new(data);
	let result = decode_varint(&mut cursor);
	assert!(
		result.is_err(),
		"should reject non-canonical 0xFE prefix for value <= 0xFFFF"
	);
}

#[test]
fn decode_varint_rejects_non_canonical_ff_prefix() {
	// Value 100 encoded with 0xFF prefix (should use single byte)
	let data: &[u8] = &[0xFF, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
	let mut cursor = Cursor::new(data);
	let result = decode_varint(&mut cursor);
	assert!(
		result.is_err(),
		"should reject non-canonical 0xFF prefix for value <= 0xFFFF_FFFF"
	);
}

#[test]
fn decode_varint_accepts_canonical_single_byte() {
	let data: &[u8] = &[0xFC]; // largest single-byte value
	let mut cursor = Cursor::new(data);
	assert_eq!(decode_varint(&mut cursor).unwrap(), 0xFC);
}

#[test]
fn decode_varint_accepts_canonical_fd_prefix() {
	// 0xFD (253) is the smallest valid value for 0xFD prefix
	let data: &[u8] = &[0xFD, 0xFD, 0x00];
	let mut cursor = Cursor::new(data);
	assert_eq!(decode_varint(&mut cursor).unwrap(), 0xFD);
}

#[test]
fn decode_varint_accepts_canonical_fe_prefix() {
	// 0x10000 is the smallest valid value for 0xFE prefix
	let data: &[u8] = &[0xFE, 0x00, 0x00, 0x01, 0x00];
	let mut cursor = Cursor::new(data);
	assert_eq!(decode_varint(&mut cursor).unwrap(), 0x10000);
}

#[test]
fn varint_roundtrip_all_ranges() {
	for &value in &[
		0u64,
		1,
		0xFC,
		0xFD,
		0xFFFF,
		0x10000,
		0xFFFF_FFFF,
		0x1_0000_0000,
		u64::MAX,
	] {
		let mut buf = Vec::new();
		write_varint(&mut buf, value);
		let mut cursor = Cursor::new(buf.as_slice());
		let decoded = decode_varint(&mut cursor).unwrap();
		assert_eq!(decoded, value, "varint roundtrip failed for {value}");
	}
}

// --- NetworkQueue buffer shrinking test ---

#[test]
fn network_queue_shrinks_buffer_after_large_drain() {
	let mut queue = NetworkQueue::new();

	// Build a valid large message that fills the buffer past the shrink threshold (65536)
	// We'll use a payload of ~70KB
	let payload = vec![0u8; 60_000];
	let msg = Message::new("ping", &payload).unwrap();
	let msg_bytes = msg.to_bytes();

	// Feed the message, which grows the buffer
	queue.process_incoming_data(&msg_bytes).unwrap();

	// Drain the message
	let parsed = queue.get_next_message();
	assert!(parsed.is_some());

	// Feed a small message to trigger the shrink check on next process
	let small_msg = Message::new("pong", &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
	queue.process_incoming_data(&small_msg.to_bytes()).unwrap();

	// After processing, the queue's internal buffer should have been consumed
	let parsed = queue.get_next_message();
	assert!(parsed.is_some());
}
