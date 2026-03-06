// SPDX-License-Identifier: Apache-2.0

//! Proof-of-work validation using scrypt(N=1024, r=1, p=1)
//!
//! Catcoin uses scrypt as its proof-of-work hash function. The `PoW` hash is
//! computed over the 80-byte serialized block header and compared against
//! the target decoded from the header's `nBits` field.
//!
//! The `PoW` hash is distinct from the identity hash (double-SHA256) used for
//! block linkage and peer communication. The scrypt hash is only computed
//! during validation and never stored

mod scrypt_native;

use std::cell::RefCell;

use crate::{difficulty::compact::compact_to_target, types::block::BlockHeader};

use scrypt_native::Scratchpad;

// Thread-local scratchpad for scrypt computation. Allocated once per thread,
// reused across all pow_hash calls. The scratchpad contents don't matter
// between calls (SMix phase 1 overwrites all entries before phase 2 reads
// any), so no clearing is needed
thread_local! {
	static SCRATCHPAD: RefCell<Box<Scratchpad>> = RefCell::new(
		Box::new([[0u32; 32]; 1024])
	);
}

/// Computes the scrypt `PoW` hash of a block header using the native implementation
///
/// The 80-byte serialized header is used as both password and salt,
/// matching the reference client's `scrypt_1024_1_1_256` function.
/// Uses a thread-local scratchpad to avoid allocating 128KB per call
pub fn pow_hash(header: &BlockHeader) -> [u8; 32] {
	let serialized = header.to_bytes();
	SCRATCHPAD.with(|pad| scrypt_native::scrypt_1024_1_1_256(&serialized, &mut pad.borrow_mut()))
}

/// Computes the scrypt `PoW` hash using the `scrypt` crate (reference implementation)
///
/// Used for verification and benchmarking against the native implementation
#[allow(clippy::expect_used, clippy::missing_panics_doc)] // scrypt params and output length are compile-time constants
pub fn pow_hash_crate(header: &BlockHeader) -> [u8; 32] {
	let serialized = header.to_bytes();
	let params = scrypt::Params::new(10, 1, 1, 32) // N=1024, r=1, p=1
		.expect("scrypt params are compile-time constants");

	let mut output = [0u8; 32];
	scrypt::scrypt(&serialized, &serialized, &params, &mut output)
		.expect("output length matches params");

	output
}

/// Checks whether a block header satisfies its claimed proof-of-work target
///
/// Returns `true` if `scrypt(header) <= target(nBits)`. The comparison is
/// done in little-endian byte order (hash bytes are already in LE from scrypt)
pub fn check_proof_of_work(header: &BlockHeader) -> bool {
	let (target, negative, overflow) = compact_to_target(header.bits);
	if negative || overflow || target.is_zero() {
		return false;
	}

	let hash = pow_hash(header);

	// Convert hash to U256 (little-endian byte order, same as scrypt output)
	let hash_u256 = crate::difficulty::U256::from_little_endian(&hash);

	hash_u256 <= target
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;
	use crate::types::hash::Hash256;

	/// Catcoin mainnet genesis block header
	const GENESIS_HEADER: BlockHeader = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::from_bytes([
			0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb,
			0x13, 0x8d, 0x1c, 0x43, 0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5,
			0x3d, 0xa3, 0x07, 0x40,
		]),
		timestamp: 1_387_838_302,
		bits: 0x1e0f_fff0,
		nonce: 588_050,
	};

	#[test]
	fn genesis_pow_hash_is_nonzero() {
		let hash = pow_hash(&GENESIS_HEADER);
		assert_ne!(hash, [0u8; 32]);
	}

	#[test]
	fn genesis_passes_pow_check() {
		assert!(check_proof_of_work(&GENESIS_HEADER));
	}

	#[test]
	fn tampered_header_fails_pow_check() {
		let mut bad = GENESIS_HEADER;
		bad.nonce = 0; // wrong nonce
		assert!(!check_proof_of_work(&bad));
	}

	#[test]
	fn zero_bits_fails_pow_check() {
		let mut bad = GENESIS_HEADER;
		bad.bits = 0;
		assert!(!check_proof_of_work(&bad));
	}

	#[test]
	fn negative_target_fails_pow_check() {
		let mut bad = GENESIS_HEADER;
		bad.bits = 0x0380_0001; // sign bit set
		assert!(!check_proof_of_work(&bad));
	}

	#[test]
	fn native_matches_crate_implementation() {
		let native = pow_hash(&GENESIS_HEADER);
		let crate_impl = pow_hash_crate(&GENESIS_HEADER);
		assert_eq!(native, crate_impl, "native scrypt must match crate output");
	}
}
