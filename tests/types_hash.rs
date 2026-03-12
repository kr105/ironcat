// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::HashSet;

use ironcat::types::hash::{HASH_LEN, Hash256, double_sha256};

#[test]
fn double_sha256_empty() {
	// SHA256d("") raw bytes are 5df6e0e2...9456 (internal wire order)
	// Display reverses to block explorer convention
	let hash = double_sha256(b"");
	assert_eq!(
		hash.to_string(),
		"56944c5d3f98413ef45cf54545538103cc9f298e0575820ad3591376e2e0f65d"
	);
}

#[test]
fn double_sha256_deterministic() {
	let data = b"catcoin is a cat";
	let hash1 = double_sha256(data);
	let hash2 = double_sha256(data);
	assert_eq!(hash1, hash2);
}

#[test]
fn hash_display_reversed_hex() {
	// Build a hash where byte 0 = 0x01, byte 1 = 0x02, ..., byte 31 = 0x20
	let mut bytes = [0u8; HASH_LEN];
	for (i, byte) in bytes.iter_mut().enumerate() {
		// i is in 0..32, fits in u8
		#[allow(clippy::cast_possible_truncation)]
		{
			*byte = (i as u8).wrapping_add(1);
		}
	}
	let hash = Hash256::from_bytes(bytes);

	// Display should reverse: byte 31 (0x20) first, byte 0 (0x01) last
	let display = hash.to_string();
	assert_eq!(
		display,
		"201f1e1d1c1b1a191817161514131211100f0e0d0c0b0a090807060504030201"
	);
}

#[test]
fn hash_equality() {
	let a = Hash256::from_bytes([0xAA; HASH_LEN]);
	let b = Hash256::from_bytes([0xAA; HASH_LEN]);
	let c = Hash256::from_bytes([0xBB; HASH_LEN]);
	assert_eq!(a, b);
	assert_ne!(a, c);
}

#[test]
fn hash_zero() {
	let zero = Hash256::ZERO;
	assert_eq!(zero.as_bytes(), &[0u8; HASH_LEN]);
	assert_eq!(
		zero.to_string(),
		"0000000000000000000000000000000000000000000000000000000000000000"
	);
}

#[test]
fn hash_as_hashset_key() {
	let mut set = HashSet::new();
	let h1 = double_sha256(b"one");
	let h2 = double_sha256(b"two");

	set.insert(h1);
	set.insert(h2);
	set.insert(h1); // duplicate

	assert_eq!(set.len(), 2);
	assert!(set.contains(&h1));
	assert!(set.contains(&h2));
}
