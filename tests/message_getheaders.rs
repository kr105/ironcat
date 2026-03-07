// SPDX-License-Identifier: Apache-2.0
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::missing_const_for_fn
)]

use ironcat::network::message_getheaders::{MessageGetHeaders, GETHEADERS_VERSION, MAX_LOCATOR_HASHES};
use ironcat::types::hash::Hash256;

fn make_hash(byte: u8) -> Hash256 {
	Hash256::from_bytes([byte; 32])
}

#[test]
fn roundtrip_single_locator() {
	let hash = make_hash(0xab);
	let msg = MessageGetHeaders::new(vec![hash], Hash256::ZERO);

	let bytes = msg.to_bytes();
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.locator_hashes(), &[hash]);
	assert_eq!(decoded.hash_stop(), Hash256::ZERO);
}

#[test]
fn roundtrip_multiple_locators() {
	let hashes = vec![make_hash(0x01), make_hash(0x02), make_hash(0x03)];
	let stop = make_hash(0xff);
	let msg = MessageGetHeaders::new(hashes.clone(), stop);

	let bytes = msg.to_bytes();
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.locator_hashes(), hashes.as_slice());
	assert_eq!(decoded.hash_stop(), stop);
}

#[test]
fn roundtrip_empty_locator() {
	let msg = MessageGetHeaders::new(vec![], Hash256::ZERO);

	let bytes = msg.to_bytes();
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.locator_hashes(), &[]);
	assert_eq!(decoded.hash_stop(), Hash256::ZERO);
}

#[test]
fn rejects_too_many_locator_hashes() {
	// Build a byte payload with hash_count = MAX_LOCATOR_HASHES + 1
	let bad_count: u64 = (MAX_LOCATOR_HASHES + 1) as u64;
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&GETHEADERS_VERSION.to_le_bytes());
	// varint for 102 (fits in one byte as 102 < 0xfd)
	bytes.push(bad_count as u8);
	// pad enough hash data so the parser doesn't fail on a read error first
	bytes.extend(vec![0u8; (bad_count as usize) * 32 + 32]);

	let result = MessageGetHeaders::from_bytes(&bytes);
	assert!(result.is_err());
	let msg = result.unwrap_err().to_string();
	assert!(msg.contains("exceeds maximum"), "unexpected error: {msg}");
}

#[test]
fn version_field_is_protocol_version() {
	let msg = MessageGetHeaders::new(vec![], Hash256::ZERO);
	let bytes = msg.to_bytes();
	let version = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
	assert_eq!(version, GETHEADERS_VERSION);
	assert_eq!(version, 70012);
}

#[test]
fn accepts_different_version_field() {
	// Peers send their negotiated version in the locator, not ours.
	// We must accept any version, matching reference client behavior
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&70003u32.to_le_bytes()); // old protocol version
	bytes.push(0); // varint: 0 locator hashes
	bytes.extend_from_slice(&[0u8; 32]); // hash_stop = zero
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.locator_hashes(), &[]);
}
