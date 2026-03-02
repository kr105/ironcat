// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::storage::{checksummed_decode, checksummed_encode};

#[test]
fn roundtrip_encode_decode() {
	let data: Vec<u8> = vec![1, 2, 3, 4, 5];
	let encoded = checksummed_encode(&data).unwrap();
	let decoded: Vec<u8> = checksummed_decode(&encoded).unwrap();
	assert_eq!(data, decoded);
}

#[test]
fn corrupt_checksum_rejected() {
	let data: Vec<u8> = vec![1, 2, 3];
	let mut encoded = checksummed_encode(&data).unwrap();
	encoded[0] ^= 0xFF;
	let result: Result<Vec<u8>, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn corrupt_payload_rejected() {
	let data: Vec<u8> = vec![1, 2, 3];
	let mut encoded = checksummed_encode(&data).unwrap();
	let last = encoded.len() - 1;
	encoded[last] ^= 0xFF;
	let result: Result<Vec<u8>, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn too_short_rejected() {
	let result: Result<Vec<u8>, _> = checksummed_decode(&[0u8; 10]);
	assert!(result.is_err());
}

#[test]
fn empty_payload_roundtrips() {
	let data: Vec<u8> = vec![];
	let encoded = checksummed_encode(&data).unwrap();
	let decoded: Vec<u8> = checksummed_decode(&encoded).unwrap();
	assert_eq!(data, decoded);
}
