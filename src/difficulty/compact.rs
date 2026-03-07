// SPDX-License-Identifier: Apache-2.0

//! Compact target encoding/decoding (nBits format)
//!
//! Catcoin encodes the 256-bit proof-of-work target in a 4-byte
//! compact format stored in block headers as `nBits`. The format uses the
//! upper byte as a byte-length exponent and the lower 3 bytes as the mantissa:
//!
//! ```text
//! nBits = [exponent: 8 bits] [mantissa: 24 bits]
//! target = mantissa * 2^(8 * (exponent - 3))
//! ```
//!
//! Bit 23 of the mantissa is the sign bit (always 0 for valid targets).
//! If the sign bit would be set by the mantissa value, the mantissa is
//! shifted right by 8 and the exponent is incremented by 1

use super::U256;

/// Expands a compact `nBits` value into a full 256-bit target
///
/// Returns `(target, negative, overflow)` matching the semantics of
/// `arith_uint256::SetCompact` in the reference client. Valid proof-of-work
/// targets always have `negative = false` and `overflow = false`
#[allow(clippy::arithmetic_side_effects)] // bit manipulation on bounded values
pub fn compact_to_target(compact: u32) -> (U256, bool, bool) {
	let size = (compact >> 24) as usize;
	let mut word = compact & 0x007f_ffff;

	let target = if size <= 3 {
		#[allow(clippy::cast_possible_truncation)] // size <= 3, fits in u32
		let shift = 8_u32.saturating_mul(3 - size as u32);
		word >>= shift;
		U256::from(word)
	} else {
		#[allow(clippy::cast_possible_truncation)] // size <= 34, fits in u32
		let shift = 8_u32.saturating_mul(size as u32 - 3);
		U256::from(word) << shift as usize
	};

	let negative = word != 0 && (compact & 0x0080_0000) != 0;
	let overflow = word != 0 && (size > 34 || (word > 0xff && size > 33) || (word > 0xffff && size > 32));

	(target, negative, overflow)
}

/// Compresses a 256-bit target into compact `nBits` format
///
/// Matches `arith_uint256::GetCompact` in the reference client.
/// Assumes the target is non-negative (sign bit is always 0)
#[allow(clippy::arithmetic_side_effects)] // bit manipulation on bounded values
pub fn target_to_compact(target: U256) -> u32 {
	let mut size = bits_u256(target).div_ceil(8);
	let mut compact: u32 = if size <= 3 {
		#[allow(clippy::cast_possible_truncation)] // masked to 24 bits below
		let c = (target.low_u64() << (8 * (3 - size))) as u32;
		c
	} else {
		let shifted = target >> (8 * (size - 3));
		#[allow(clippy::cast_possible_truncation)] // masked to 24 bits below
		let c = shifted.low_u64() as u32;
		c
	};

	// If the sign bit (bit 23) is set, shift the mantissa right and bump
	// the exponent so the sign bit stays clear
	if compact & 0x0080_0000 != 0 {
		compact >>= 8;
		size += 1;
	}

	compact &= 0x007f_ffff;
	#[allow(clippy::cast_possible_truncation)] // size <= 35, fits in u32
	{
		compact |= (size as u32) << 24;
	}
	compact
}

/// Returns the number of bits needed to represent the value (position of
/// the highest set bit + 1). Returns 0 for zero
///
/// Matches `base_uint<256>::bits()` in the reference client
pub fn bits_u256(val: U256) -> usize {
	// U256 is stored as [u64; 4] in little-endian word order
	// (index 0 = least significant)
	for pos in (0..4).rev() {
		#[allow(clippy::indexing_slicing)] // pos is in 0..4, U256 has exactly 4 u64 words
		let word = val.0[pos];
		if word != 0 {
			#[allow(clippy::arithmetic_side_effects)] // leading_zeros <= 63 for non-zero u64
			let bit_pos = 63 - word.leading_zeros() as usize;
			#[allow(clippy::arithmetic_side_effects)] // pos <= 3, bit_pos <= 63
			return 64 * pos + bit_pos + 1;
		}
	}
	0
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used)]
	use super::*;

	#[test]
	fn genesis_bits_roundtrip() {
		// Catcoin genesis nBits = 0x1e0ffff0
		let (target, neg, ovf) = compact_to_target(0x1e0f_fff0);
		assert!(!neg);
		assert!(!ovf);
		assert!(target > U256::zero());

		let compact = target_to_compact(target);
		assert_eq!(compact, 0x1e0f_fff0);
	}

	#[test]
	fn hardcoded_difficulty_16_roundtrip() {
		// CIP01 transition: hardcoded 0x1c0ffff0
		let (target, neg, ovf) = compact_to_target(0x1c0f_fff0);
		assert!(!neg);
		assert!(!ovf);

		let compact = target_to_compact(target);
		assert_eq!(compact, 0x1c0f_fff0);
	}

	#[test]
	fn zero_target() {
		let (target, _, _) = compact_to_target(0);
		assert_eq!(target, U256::zero());
		assert_eq!(target_to_compact(U256::zero()), 0);
	}

	#[test]
	fn small_exponent() {
		// 0x03000001: size=3, mantissa=1 -> target=1
		let (target, neg, ovf) = compact_to_target(0x0300_0001);
		assert!(!neg);
		assert!(!ovf);
		assert_eq!(target, U256::from(1));
		// Roundtrip is not exact for non-canonical compact values
		// Encoding 1 produces a canonical form with size=1
	}

	#[test]
	fn negative_flag() {
		// Sign bit set with non-zero mantissa
		let compact = 0x0380_0001; // size=3, mantissa has sign bit
		let (_, neg, _) = compact_to_target(compact);
		assert!(neg);
	}

	#[test]
	fn overflow_detection() {
		// size=35 with non-zero mantissa -> overflow
		let compact = 0x2300_0001;
		let (_, _, ovf) = compact_to_target(compact);
		assert!(ovf);
	}

	#[test]
	fn bits_u256_zero() {
		assert_eq!(bits_u256(U256::zero()), 0);
	}

	#[test]
	fn bits_u256_one() {
		assert_eq!(bits_u256(U256::from(1)), 1);
	}

	#[test]
	fn bits_u256_powers_of_two() {
		assert_eq!(bits_u256(U256::from(128)), 8);
		assert_eq!(bits_u256(U256::from(256)), 9);
		assert_eq!(bits_u256(U256::from(0xffff_u64)), 16);
	}
}
