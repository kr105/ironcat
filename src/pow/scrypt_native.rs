// SPDX-License-Identifier: Apache-2.0

//! Hand-optimized scrypt(N=1024, r=1, p=1, dkLen=32) for `PoW` validation
//!
//! Specialized for Catcoin's exact parameters with these key optimizations:
//!
//! - **u32 scratchpad**: `SMix` operates on `[u32; 32]` natively, so the 128KB
//!   scratchpad is `[[u32; 32]; 1024]` instead of `[u8]`. This eliminates
//!   65,536 byte-to-u32 conversions per hash (only the initial load and final
//!   store touch bytes)
//! - **SHA256 midstate reuse**: HMAC-SHA256 inner/outer pads are exactly one
//!   SHA256 block (64 bytes). The hasher state after absorbing the pad is
//!   computed once and cloned for each PBKDF2 block, saving 10 SHA256
//!   compressions per hash
//! - **Zero heap allocation**: HMAC key uses a stack buffer; the scratchpad
//!   is caller-provided (intended for thread-local reuse)
//! - **Minimal copies in `BlockMix`**: writes the even block directly to its
//!   final position, avoiding an intermediate buffer
//!
//! The algorithm:
//! 1. PBKDF2-SHA256(password=header, salt=header, c=1, dkLen=128) -> B
//! 2. SMix(B, N=1024, scratchpad)
//! 3. PBKDF2-SHA256(password=header, salt=B, c=1, dkLen=32) -> output

use sha2::{Digest, Sha256};

/// scrypt cost parameter: 2^10 = 1024
const N: usize = 1024;

/// Scratchpad type: N entries of 32 u32s (128 bytes each, 128KB total)
///
/// Using `[u32; 32]` entries instead of `[u8; 128]` avoids byte-to-u32
/// conversion on every scratchpad read/write in `SMix`. On little-endian
/// platforms the memory layout is identical
pub type Scratchpad = [[u32; 32]; N];

/// Computes scrypt(1024,1,1,32) using a caller-provided scratchpad
///
/// The `scratchpad` is overwritten during computation. Its initial contents
/// do not matter (phase 1 fills all entries before phase 2 reads any),
/// so callers can safely reuse the same scratchpad across calls without
/// clearing it
#[allow(
	clippy::arithmetic_side_effects,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation
)]
// Arithmetic is bounded by fixed scrypt parameters; indices are within
// known-size arrays; truncation is intentional for u32 wrapping math
pub fn scrypt_1024_1_1_256(input: &[u8], scratchpad: &mut Scratchpad) -> [u8; 32] {
	// Step 1: PBKDF2-SHA256(password=input, salt=input, c=1, dkLen=128) -> b
	let mut b = [0u8; 128];
	pbkdf2_sha256_single(input, input, &mut b);

	// Step 2: SMix(b, N=1024, scratchpad)
	smix(&mut b, scratchpad);

	// Step 3: PBKDF2-SHA256(password=input, salt=b, c=1, dkLen=32) -> output
	let mut output = [0u8; 32];
	pbkdf2_sha256_single(input, &b, &mut output);

	output
}

/// PBKDF2-SHA256 with iteration count = 1 (specialized for scrypt's usage)
///
/// With c=1, PBKDF2 reduces to:
///   DK = T1 || T2 || ...
///   `Ti = HMAC-SHA256(password, salt || INT_BE(i))`
///
/// Optimizations over a naive implementation:
/// - HMAC key derived into a 64-byte stack buffer (no heap allocation)
/// - SHA256 state after absorbing the 64-byte ipad/opad is computed once
///   and cloned for each block, saving one SHA256 compression per block
#[allow(
	clippy::arithmetic_side_effects,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation
)]
// Arithmetic bounded by block count from fixed output sizes (32 or 128 bytes);
// indexing bounded by key length <= 64 and block indices within output;
// truncation: blocks_needed <= 4, fits in u32
fn pbkdf2_sha256_single(password: &[u8], salt: &[u8], output: &mut [u8]) {
	let blocks_needed = (output.len() + 31) / 32;

	// Derive HMAC key into a stack buffer (no heap allocation).
	// HMAC spec: if key > block size (64), hash it first
	let mut key_buf = [0u8; 64];
	let key_len = if password.len() > 64 {
		let h = Sha256::digest(password);
		key_buf[..32].copy_from_slice(&h);
		32
	} else {
		key_buf[..password.len()].copy_from_slice(password);
		password.len()
	};

	// XOR key into HMAC inner/outer pads
	let mut ipad = [0x36u8; 64];
	let mut opad = [0x5cu8; 64];
	for i in 0..key_len {
		ipad[i] ^= key_buf[i];
		opad[i] ^= key_buf[i];
	}

	// Precompute SHA256 midstates: ipad and opad are each exactly 64 bytes
	// (one SHA256 block), so the hasher state after absorbing them is fixed.
	// Cloning this state for each PBKDF2 block avoids re-hashing the pads
	let mut inner_base = Sha256::new();
	inner_base.update(ipad);
	let mut outer_base = Sha256::new();
	outer_base.update(opad);

	for block_idx in 1..=blocks_needed {
		// HMAC-SHA256(key, salt || INT_BE(block_idx)):
		//   inner = SHA256(ipad || salt || block_idx_be)
		//   outer = SHA256(opad || inner)
		let mut inner = inner_base.clone();
		inner.update(salt);
		inner.update((block_idx as u32).to_be_bytes());
		let inner_hash = inner.finalize();

		let mut outer = outer_base.clone();
		outer.update(inner_hash);
		let t = outer.finalize();

		// Copy to output (last block may be partial for dkLen not multiple of 32)
		let start = (block_idx - 1) * 32;
		let end = output.len().min(start + 32);
		output[start..end].copy_from_slice(&t[..end - start]);
	}
}

/// `SMix`: the memory-hard mixing function
///
/// Operates on a 128-byte block B (as bytes), using a u32 scratchpad V.
/// The byte-to-u32 conversion happens only on entry and exit; all 2048
/// iterations of fill + lookup operate entirely on native u32 arrays
#[allow(
	clippy::arithmetic_side_effects,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation
)]
// Arithmetic bounded by fixed N=1024; indices bounded by known array sizes;
// truncation intentional for u32 mod N
fn smix(b: &mut [u8; 128], v: &mut Scratchpad) {
	let mut x = [0u32; 32];

	// Load B into X as little-endian u32s (only byte conversion in the function)
	for i in 0..32 {
		x[i] = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
	}

	// Phase 1: fill scratchpad with sequential BlockMix results.
	// Direct u32 array copy -- no byte conversion needed
	for entry in v.iter_mut() {
		*entry = x;
		block_mix_salsa8(&mut x);
	}

	// Phase 2: random lookups into scratchpad.
	// Index derived from X[16] mod N (the first u32 of the second 64-byte chunk).
	// XOR is direct u32-to-u32, no byte conversion
	for _ in 0..N {
		let j = (x[16] & (N as u32 - 1)) as usize;
		for k in 0..32 {
			x[k] ^= v[j][k];
		}
		block_mix_salsa8(&mut x);
	}

	// Store X back into B as little-endian bytes (only byte conversion on exit)
	for i in 0..32 {
		let bytes = x[i].to_le_bytes();
		b[i * 4] = bytes[0];
		b[i * 4 + 1] = bytes[1];
		b[i * 4 + 2] = bytes[2];
		b[i * 4 + 3] = bytes[3];
	}
}

/// `BlockMix` with Salsa20/8 for r=1 (two 64-byte chunks)
///
/// For r=1, the input X has two 16-u32 halves: B\[0\] = x\[0..16\], B\[1\] = x\[16..32\].
/// The algorithm:
///   1. T = B\[1\] ^ B\[0\], then Salsa8(T) -> even block
///   2. T = T ^ B\[1\], then Salsa8(T) -> odd block
///   3. Output = \[even, odd\]
///
/// The even block is written directly to x\[0..16\] (already consumed in step 1),
/// avoiding an intermediate copy
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// Indices bounded by fixed array sizes (16 and 32 elements)
#[inline]
fn block_mix_salsa8(x: &mut [u32; 32]) {
	// Start with T = B[1] (second half)
	let mut t = [0u32; 16];
	t.copy_from_slice(&x[16..32]);

	// T ^= B[0], then Salsa8(T) -> even block result
	for i in 0..16 {
		t[i] ^= x[i];
	}
	salsa20_8_core(&mut t);

	// Write even block directly to x[0..16] -- B[0] is already consumed above
	// and B[1] in x[16..32] is still intact for the next step
	x[..16].copy_from_slice(&t);

	// T ^= B[1] (still in x[16..32]), then Salsa8(T) -> odd block result
	for i in 0..16 {
		t[i] ^= x[16 + i];
	}
	salsa20_8_core(&mut t);

	// Write odd block to x[16..32]
	x[16..32].copy_from_slice(&t);
}

/// Salsa20/8 core: 8 rounds (4 double-rounds) of quarter-round mixing
///
/// Each double-round applies column operations then row operations on 16 u32s.
/// The operations (wrapping add + rotate left + XOR) map directly to native
/// CPU instructions. After all rounds, the original input is added back
/// (feedforward) to make the function non-invertible
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
// Arithmetic uses wrapping_add (no overflow); indices are constants 0..15
#[inline]
fn salsa20_8_core(b: &mut [u32; 16]) {
	// Copy input for feedforward at the end
	let mut x = *b;

	// 8 rounds = 4 double-rounds (column + row)
	for _ in 0..4 {
		// Column round: each quarter-round mixes one column of the 4x4 matrix
		//   matrix layout:  [ 0  1  2  3]
		//                   [ 4  5  6  7]
		//                   [ 8  9 10 11]
		//                   [12 13 14 15]
		x[4] ^= x[0].wrapping_add(x[12]).rotate_left(7);
		x[8] ^= x[4].wrapping_add(x[0]).rotate_left(9);
		x[12] ^= x[8].wrapping_add(x[4]).rotate_left(13);
		x[0] ^= x[12].wrapping_add(x[8]).rotate_left(18);

		x[9] ^= x[5].wrapping_add(x[1]).rotate_left(7);
		x[13] ^= x[9].wrapping_add(x[5]).rotate_left(9);
		x[1] ^= x[13].wrapping_add(x[9]).rotate_left(13);
		x[5] ^= x[1].wrapping_add(x[13]).rotate_left(18);

		x[14] ^= x[10].wrapping_add(x[6]).rotate_left(7);
		x[2] ^= x[14].wrapping_add(x[10]).rotate_left(9);
		x[6] ^= x[2].wrapping_add(x[14]).rotate_left(13);
		x[10] ^= x[6].wrapping_add(x[2]).rotate_left(18);

		x[3] ^= x[15].wrapping_add(x[11]).rotate_left(7);
		x[7] ^= x[3].wrapping_add(x[15]).rotate_left(9);
		x[11] ^= x[7].wrapping_add(x[3]).rotate_left(13);
		x[15] ^= x[11].wrapping_add(x[7]).rotate_left(18);

		// Row round: each quarter-round mixes one row of the 4x4 matrix
		x[1] ^= x[0].wrapping_add(x[3]).rotate_left(7);
		x[2] ^= x[1].wrapping_add(x[0]).rotate_left(9);
		x[3] ^= x[2].wrapping_add(x[1]).rotate_left(13);
		x[0] ^= x[3].wrapping_add(x[2]).rotate_left(18);

		x[6] ^= x[5].wrapping_add(x[4]).rotate_left(7);
		x[7] ^= x[6].wrapping_add(x[5]).rotate_left(9);
		x[4] ^= x[7].wrapping_add(x[6]).rotate_left(13);
		x[5] ^= x[4].wrapping_add(x[7]).rotate_left(18);

		x[11] ^= x[10].wrapping_add(x[9]).rotate_left(7);
		x[8] ^= x[11].wrapping_add(x[10]).rotate_left(9);
		x[9] ^= x[8].wrapping_add(x[11]).rotate_left(13);
		x[10] ^= x[9].wrapping_add(x[8]).rotate_left(18);

		x[12] ^= x[15].wrapping_add(x[14]).rotate_left(7);
		x[13] ^= x[12].wrapping_add(x[15]).rotate_left(9);
		x[14] ^= x[13].wrapping_add(x[12]).rotate_left(13);
		x[15] ^= x[14].wrapping_add(x[13]).rotate_left(18);
	}

	// Feedforward: add original input to make the function non-invertible
	for i in 0..16 {
		b[i] = b[i].wrapping_add(x[i]);
	}
}
