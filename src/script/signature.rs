// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;

use k256::ecdsa::signature::hazmat::PrehashVerifier;
use k256::ecdsa::{Signature, VerifyingKey};

use crate::network::write_varint;
use crate::types::hash::{double_sha256, Hash256};
use crate::types::transaction::Transaction;

use super::ScriptError;

/// Opcode byte for `OP_CODESEPARATOR`
const OP_CODESEPARATOR: u8 = 0xab;

/// Signature hash types that control which parts of a transaction are signed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigHashType {
	/// Sign all inputs and all outputs (0x01)
	All,
	/// Sign all inputs but no outputs (0x02)
	None,
	/// Sign all inputs and only the output at the same index (0x03)
	Single,
	/// Sign only the current input and all outputs (0x81)
	AllAnyoneCanPay,
	/// Sign only the current input and no outputs (0x82)
	NoneAnyoneCanPay,
	/// Sign only the current input and only the matching output (0x83)
	SingleAnyoneCanPay,
}

impl SigHashType {
	/// Parses a sighash type from a raw byte
	///
	/// Returns `None` for unrecognized values
	pub const fn from_byte(byte: u8) -> Option<Self> {
		match byte {
			0x01 => Some(Self::All),
			0x02 => Some(Self::None),
			0x03 => Some(Self::Single),
			0x81 => Some(Self::AllAnyoneCanPay),
			0x82 => Some(Self::NoneAnyoneCanPay),
			0x83 => Some(Self::SingleAnyoneCanPay),
			_ => Option::None,
		}
	}

	/// Returns the base hash type (1, 2, or 3) with ANYONECANPAY stripped
	pub const fn base_type(&self) -> u8 {
		match self {
			Self::All | Self::AllAnyoneCanPay => 1,
			Self::None | Self::NoneAnyoneCanPay => 2,
			Self::Single | Self::SingleAnyoneCanPay => 3,
		}
	}

	/// Returns true if the ANYONECANPAY flag is set
	pub const fn is_anyonecanpay(&self) -> bool {
		matches!(
			self,
			Self::AllAnyoneCanPay | Self::NoneAnyoneCanPay | Self::SingleAnyoneCanPay
		)
	}
}

/// Removes all occurrences of a serialized push of `data` from a script
///
/// This implements Bitcoin's `FindAndDelete` for the legacy sighash algorithm.
/// The target pattern is the data serialized as a script push operation.
/// Handles direct push (len <= 75), PUSHDATA1, PUSHDATA2, and PUSHDATA4
/// encodings as raw byte subsequences, matching Bitcoin Core behavior
// Pattern matching and memcpy with indices guarded by length checks
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn find_and_delete(script: &[u8], data: &[u8]) -> Vec<u8> {
	if data.is_empty() {
		return script.to_vec();
	}

	// Build all possible push encodings for this data
	let patterns = build_push_patterns(data);

	let mut result = Vec::with_capacity(script.len());
	let mut i = 0;
	'outer: while i < script.len() {
		for pattern in &patterns {
			if i + pattern.len() <= script.len() && script[i..i + pattern.len()] == pattern[..] {
				// Skip this occurrence
				i += pattern.len();
				continue 'outer;
			}
		}
		result.push(script[i]);
		i += 1;
	}
	result
}

/// Builds all valid push-encoding patterns for the given data
///
/// Returns patterns for each encoding that can represent this data length:
/// - Direct push (1-75 bytes): `[len] [data]`
/// - PUSHDATA1 (1-255 bytes): `[0x4c] [len_u8] [data]`
/// - PUSHDATA2 (1-65535 bytes): `[0x4d] [len_u16_le] [data]`
/// - PUSHDATA4 (any size): `[0x4e] [len_u32_le] [data]`
#[allow(clippy::cast_possible_truncation, clippy::arithmetic_side_effects)] // lengths are checked against encoding limits; capacity additions are small constants + bounded len
fn build_push_patterns(data: &[u8]) -> Vec<Vec<u8>> {
	let len = data.len();
	let mut patterns = Vec::with_capacity(4);

	// Direct push: len 1..=75
	if len <= 75 {
		let mut p = Vec::with_capacity(1 + len);
		p.push(len as u8);
		p.extend_from_slice(data);
		patterns.push(p);
	}

	// PUSHDATA1: len 0..=255
	if len <= 255 {
		let mut p = Vec::with_capacity(2 + len);
		p.push(0x4c); // OP_PUSHDATA1
		p.push(len as u8);
		p.extend_from_slice(data);
		patterns.push(p);
	}

	// PUSHDATA2: len 0..=65535
	if len <= 65535 {
		let mut p = Vec::with_capacity(3 + len);
		p.push(0x4d); // OP_PUSHDATA2
		p.extend_from_slice(&(len as u16).to_le_bytes());
		p.extend_from_slice(data);
		patterns.push(p);
	}

	// PUSHDATA4: any length
	{
		let mut p = Vec::with_capacity(5 + len);
		p.push(0x4e); // OP_PUSHDATA4
		#[allow(clippy::cast_possible_truncation)] // script data can't exceed u32::MAX
		p.extend_from_slice(&(len as u32).to_le_bytes());
		p.extend_from_slice(data);
		patterns.push(p);
	}

	patterns
}

/// Removes `OP_CODESEPARATOR` opcodes from a script
///
/// Walks the script properly, skipping over push data so that 0xab bytes
/// inside push payloads are not removed
// Script parsing is dense byte-level code where indexing and arithmetic are
// guarded by explicit bounds checks at each branch
#[allow(
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]
fn remove_codeseparators(script: &[u8]) -> Vec<u8> {
	let mut result = Vec::with_capacity(script.len());
	let mut i = 0;
	while i < script.len() {
		let op = script[i];
		if op == OP_CODESEPARATOR {
			i += 1;
			continue;
		}

		if op <= 0x4b {
			// OP_PUSHBYTES_N: push next `op` bytes
			let end = (i + 1 + op as usize).min(script.len());
			result.extend_from_slice(&script[i..end]);
			i = end;
		} else if op == 0x4c {
			// OP_PUSHDATA1: next byte is length
			let len_pos = i + 1;
			if len_pos >= script.len() {
				result.push(op);
				break;
			}
			let end = (i + 2 + script[len_pos] as usize).min(script.len());
			result.extend_from_slice(&script[i..end]);
			i = end;
		} else if op == 0x4d {
			// OP_PUSHDATA2: next 2 bytes are length (LE)
			if i + 3 > script.len() {
				result.extend_from_slice(&script[i..]);
				break;
			}
			let data_len = u16::from_le_bytes([script[i + 1], script[i + 2]]) as usize;
			let end = (i + 3 + data_len).min(script.len());
			result.extend_from_slice(&script[i..end]);
			i = end;
		} else if op == 0x4e {
			// OP_PUSHDATA4: next 4 bytes are length (LE)
			if i + 5 > script.len() {
				result.extend_from_slice(&script[i..]);
				break;
			}
			let data_len = u32::from_le_bytes([script[i + 1], script[i + 2], script[i + 3], script[i + 4]]) as usize;
			let end = (i + 5 + data_len).min(script.len());
			result.extend_from_slice(&script[i..end]);
			i = end;
		} else {
			// Regular opcode, just copy it
			result.push(op);
			i += 1;
		}
	}
	result
}

/// Computes the signature hash for a transaction input
///
/// This is the legacy Bitcoin sighash algorithm. The transaction is serialized
/// with modifications based on the hash type, then double-SHA256'd.
///
/// `hash_type` is the raw sighash byte from the signature (last byte of the
/// DER-encoded sig). The base type is extracted as `hash_type & 0x1f`:
/// NONE (2) and SINGLE (3) have special output handling, everything else
/// (including ALL=1 and any unknown value) serializes all outputs.
/// The ANYONECANPAY flag is `hash_type & 0x80`. The raw byte is appended
/// as a 4-byte LE integer at the end of the serialized data.
///
/// The caller must ensure `input_idx` is valid for `tx.vin`
// Sighash serialization involves dense index arithmetic and casts that are all
// bounded by transaction field lengths validated upstream
#[allow(
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]
pub fn signature_hash(tx: &Transaction, input_idx: usize, script_code: &[u8], hash_type: u8) -> Hash256 {
	let base = hash_type & 0x1f;
	let anyonecanpay = hash_type & 0x80 != 0;

	// SIGHASH_SINGLE bug: if input_idx >= number of outputs, return the
	// historical hardcoded hash
	if base == 3 && input_idx >= tx.vout.len() {
		let mut one = [0u8; 32];
		one[0] = 0x01;
		return Hash256::from_bytes(one);
	}

	let script_code = remove_codeseparators(script_code);
	let mut buf = Vec::with_capacity(256);

	// Version
	buf.extend_from_slice(&tx.version.to_le_bytes());

	// Inputs
	if anyonecanpay {
		// Only serialize the signing input
		write_varint(&mut buf, 1);
		buf.extend_from_slice(tx.vin[input_idx].prev_output.txid.as_bytes());
		buf.extend_from_slice(&tx.vin[input_idx].prev_output.index.to_le_bytes());
		write_varint(&mut buf, script_code.len() as u64);
		buf.extend_from_slice(&script_code);
		buf.extend_from_slice(&tx.vin[input_idx].sequence.to_le_bytes());
	} else {
		write_varint(&mut buf, tx.vin.len() as u64);
		for (i, input) in tx.vin.iter().enumerate() {
			buf.extend_from_slice(input.prev_output.txid.as_bytes());
			buf.extend_from_slice(&input.prev_output.index.to_le_bytes());

			if i == input_idx {
				// Signing input gets the script_code
				write_varint(&mut buf, script_code.len() as u64);
				buf.extend_from_slice(&script_code);
			} else {
				// Other inputs get empty script
				write_varint(&mut buf, 0);
			}

			// Sequence: zeroed for non-signing inputs when base is None or Single
			if (base == 2 || base == 3) && i != input_idx {
				buf.extend_from_slice(&0u32.to_le_bytes());
			} else {
				buf.extend_from_slice(&input.sequence.to_le_bytes());
			}
		}
	}

	// Outputs -- check NONE and SINGLE explicitly, everything else
	// (including ALL and any unknown base type) serializes all outputs.
	// This matches Bitcoin Core's SignatureHash behavior
	match base {
		2 => {
			// NONE: no outputs
			write_varint(&mut buf, 0);
		}
		3 => {
			// SINGLE: outputs up to and including input_idx
			write_varint(&mut buf, (input_idx + 1) as u64);
			for (i, output) in tx.vout.iter().enumerate() {
				match i.cmp(&input_idx) {
					Ordering::Less => {
						// Blank output: value -1, empty script
						buf.extend_from_slice(&(-1i64).to_le_bytes());
						write_varint(&mut buf, 0);
					}
					Ordering::Equal => {
						buf.extend_from_slice(&output.value.to_le_bytes());
						write_varint(&mut buf, output.script_pubkey.len() as u64);
						buf.extend_from_slice(&output.script_pubkey);
					}
					Ordering::Greater => break,
				}
			}
		}
		_ => {
			// ALL (and any unknown base type): serialize all outputs
			write_varint(&mut buf, tx.vout.len() as u64);
			for output in &tx.vout {
				buf.extend_from_slice(&output.value.to_le_bytes());
				write_varint(&mut buf, output.script_pubkey.len() as u64);
				buf.extend_from_slice(&output.script_pubkey);
			}
		}
	}

	// Locktime
	buf.extend_from_slice(&tx.locktime.to_le_bytes());

	// Hash type as 4-byte LE u32 (raw byte, not normalized)
	buf.extend_from_slice(&u32::from(hash_type).to_le_bytes());

	double_sha256(&buf)
}

/// Validates that a signature follows strict DER encoding per BIP66
///
/// An empty signature is valid (used as a no-op padding in `OP_CHECKMULTISIG`).
/// The last byte is the sighash type and is not part of the DER structure
// DER validation is dense byte-level parsing where every index access is
// guarded by the length checks that precede it
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn check_signature_encoding(sig: &[u8]) -> Result<(), ScriptError> {
	if sig.is_empty() {
		return Ok(());
	}

	if sig.len() < 9 || sig.len() > 73 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// First byte must be compound structure marker
	if sig[0] != 0x30 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// total_len must equal sig.len() - 3 (0x30, len byte, hashtype)
	let total_len = sig[1] as usize;
	if total_len != sig.len() - 3 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// R integer
	if sig[2] != 0x02 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}
	let r_len = sig[3] as usize;
	if r_len == 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// Check R is not negative (high bit set without 0x00 prefix)
	if sig[4] & 0x80 != 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// Check R has no unnecessary leading zeros
	if r_len > 1 && sig[4] == 0x00 && sig[5] & 0x80 == 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// S integer marker
	let s_marker_pos = 4 + r_len;
	if s_marker_pos >= sig.len() || sig[s_marker_pos] != 0x02 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	let s_len_pos = s_marker_pos + 1;
	if s_len_pos >= sig.len() {
		return Err(ScriptError::InvalidSignatureEncoding);
	}
	let s_len = sig[s_len_pos] as usize;
	if s_len == 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	let s_start = s_len_pos + 1;
	if s_start >= sig.len() {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// Check S is not negative
	if sig[s_start] & 0x80 != 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// Check S has no unnecessary leading zeros
	if s_len > 1 && sig[s_start] == 0x00 && s_start + 1 < sig.len() && sig[s_start + 1] & 0x80 == 0 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// r_len + s_len + 7 must equal sig.len()
	if r_len + s_len + 7 != sig.len() {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	// Last byte must be a valid hashtype
	let hashtype_byte = sig[sig.len() - 1];
	if SigHashType::from_byte(hashtype_byte).is_none() {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	Ok(())
}

/// Checks that the S value in a DER-encoded signature is low (BIP62 rule 5)
///
/// The S value must be <= secp256k1 order / 2. An empty signature is valid
// Byte-level DER extraction with indices guarded by the structure validated
// by check_signature_encoding (caller is expected to validate first)
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn check_low_s(sig: &[u8]) -> Result<(), ScriptError> {
	// Half of the secp256k1 group order
	const HALF_ORDER: [u8; 32] = [
		0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x5D, 0x57,
		0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
	];

	if sig.is_empty() {
		return Ok(());
	}

	// Extract S: skip 0x30, total_len, 0x02, r_len, R bytes, 0x02, s_len
	let r_len = sig[3] as usize;
	let s_len = sig[4 + r_len + 1] as usize;
	let s_start = 4 + r_len + 2;
	let s_bytes = &sig[s_start..s_start + s_len];

	// Compare S (big-endian) against half_order
	// Pad S to 32 bytes for comparison
	if s_len > 32 {
		return Err(ScriptError::InvalidSignatureEncoding);
	}

	let mut s_padded = [0u8; 32];
	s_padded[32 - s_len..].copy_from_slice(s_bytes);

	// If S > half_order, reject
	for i in 0..32 {
		match s_padded[i].cmp(&HALF_ORDER[i]) {
			Ordering::Greater => return Err(ScriptError::InvalidSignatureEncoding),
			Ordering::Less => return Ok(()),
			Ordering::Equal => {}
		}
	}

	// S == half_order is still valid (<=)
	Ok(())
}

/// Validates that a public key has the correct encoding
///
/// Compressed keys are 33 bytes starting with 0x02 or 0x03.
/// Uncompressed keys are 65 bytes starting with 0x04.
/// An empty key is invalid
// All index accesses are guarded by the length checks
#[allow(clippy::indexing_slicing)]
pub fn check_pubkey_encoding(pubkey: &[u8]) -> Result<(), ScriptError> {
	if pubkey.is_empty() {
		return Err(ScriptError::InvalidPubKey);
	}

	match pubkey[0] {
		0x02 | 0x03 if pubkey.len() == 33 => Ok(()),
		0x04 if pubkey.len() == 65 => Ok(()),
		_ => Err(ScriptError::InvalidPubKey),
	}
}

/// Verifies an ECDSA signature against a public key and message hash
///
/// Returns true if the signature is valid, false otherwise. The last byte
/// of `sig` is the hashtype byte and is excluded from the DER data.
///
/// Parses DER leniently (tolerating extra leading zeros, non-minimal
/// lengths, and trailing garbage) to match pre-BIP66 OpenSSL behavior.
/// Catcoin has not activated BIP66 (strict DER) at consensus level
pub fn verify_ecdsa(sig: &[u8], pubkey: &[u8], sighash: &Hash256) -> bool {
	if sig.is_empty() {
		return false;
	}

	let Some(der_sig) = sig.get(..sig.len().saturating_sub(1)) else {
		return false;
	};

	let Some(signature) = parse_der_lenient(der_sig) else {
		return false;
	};

	// Normalize to low-S if needed. k256's verify_prehash only accepts
	// low-S signatures, but pre-BIP62 Bitcoin (and Catcoin, which never
	// activated BIP62) allows high-S. normalize_s() returns None if S
	// is already low
	let signature = signature.normalize_s().unwrap_or(signature);

	let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(pubkey) else {
		return false;
	};

	// Use verify_prehash because the sighash is already double-SHA256'd.
	// The regular verify() would hash it again with SHA-256, producing a
	// wrong digest. Bitcoin ECDSA signs/verifies raw 32-byte hashes
	verifying_key.verify_prehash(sighash.as_bytes(), &signature).is_ok()
}

/// Parses a DER-encoded ECDSA signature leniently
///
/// k256's `Signature::from_der()` enforces strict BIP66-level DER validation,
/// rejecting signatures that pre-BIP66 nodes accepted via OpenSSL's lenient
/// parser. Since Catcoin has `BIP66Height` set to `INT_MAX` (never activated),
/// real chain signatures may use non-strict DER that `from_der()` would reject.
///
/// This function extracts R and S as raw integers from the DER structure and
/// constructs the signature via `Signature::from_scalars()`, which only cares
/// about the scalar values, not the encoding. No existing crate provides a
/// lenient DER parser for ECDSA -- this is a Bitcoin-specific historical
/// requirement from the OpenSSL era.
///
/// Tolerates encodings that strict BIP66 would reject:
/// - Extra leading zeros in R or S (e.g. `[0x00, 0x00, 0x01]` for value 1)
/// - Wrong SEQUENCE total length byte
/// - Trailing garbage after the S integer
/// - Non-minimal DER length encoding (long form for small lengths)
#[allow(clippy::arithmetic_side_effects)] // pos is bounded by der.len() via .get() checks
fn parse_der_lenient(der: &[u8]) -> Option<Signature> {
	let mut pos = 0;

	// SEQUENCE tag
	if *der.get(pos)? != 0x30 {
		return None;
	}
	pos += 1;

	// Total length -- read but don't validate against actual content
	let _total_len = read_der_len(der, &mut pos)?;

	// R integer
	let r_bytes = read_der_integer(der, &mut pos)?;

	// S integer
	let s_bytes = read_der_integer(der, &mut pos)?;

	// Trailing bytes after S are ignored (OpenSSL leniency)

	let r = scalar_from_bytes(&r_bytes)?;
	let s = scalar_from_bytes(&s_bytes)?;

	Signature::from_scalars(r, s).ok()
}

/// Reads a DER length field, handling both short and long forms
#[allow(clippy::arithmetic_side_effects)]
fn read_der_len(data: &[u8], pos: &mut usize) -> Option<usize> {
	let b = *data.get(*pos)?;
	*pos += 1;
	if b & 0x80 == 0 {
		Some(b as usize)
	} else {
		let n = (b & 0x7f) as usize;
		if n == 0 || n > 4 {
			return None;
		}
		let mut len = 0usize;
		for _ in 0..n {
			len = len.checked_shl(8)?.checked_add(usize::from(*data.get(*pos)?))?;
			*pos += 1;
		}
		Some(len)
	}
}

/// Reads a DER INTEGER (tag 0x02 + length + value bytes)
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // bounds checked before access
fn read_der_integer(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
	if *data.get(*pos)? != 0x02 {
		return None;
	}
	*pos += 1;

	let len = read_der_len(data, pos)?;
	if len == 0 {
		return None;
	}
	let end = pos.checked_add(len)?;
	if end > data.len() {
		return None;
	}

	let bytes = data[*pos..end].to_vec();
	*pos = end;
	Some(bytes)
}

/// Converts raw DER integer bytes to a 32-byte big-endian scalar
///
/// Strips leading zeros and right-aligns into a 32-byte buffer for
/// use with `Signature::from_scalars`
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // guarded by length check
fn scalar_from_bytes(bytes: &[u8]) -> Option<[u8; 32]> {
	// Strip leading zeros, keeping at least the last byte
	let start = bytes
		.iter()
		.position(|&b| b != 0)
		.unwrap_or_else(|| bytes.len().saturating_sub(1));
	let significant = bytes.get(start..)?;
	if significant.len() > 32 {
		return None;
	}
	let mut buf = [0u8; 32];
	buf[32 - significant.len()..].copy_from_slice(significant);
	Some(buf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects, clippy::indexing_slicing)]
mod tests {
	use super::*;

	#[test]
	fn remove_codeseparators_standalone() {
		// 0xab alone should be removed
		let script = vec![0x76, 0xab, 0xa9];
		assert_eq!(remove_codeseparators(&script), vec![0x76, 0xa9]);
	}

	#[test]
	fn remove_codeseparators_inside_pushdata() {
		// 0x01 0xab = push 1 byte (0xab) -- should NOT be removed
		let script = vec![0x76, 0x01, 0xab, 0xa9];
		assert_eq!(remove_codeseparators(&script), vec![0x76, 0x01, 0xab, 0xa9]);
	}

	#[test]
	fn remove_codeseparators_empty() {
		assert_eq!(remove_codeseparators(&[]), Vec::<u8>::new());
	}

	// --- Lenient DER parser tests ---

	/// Builds a strict DER signature with the given R and S byte values
	fn build_der(r: &[u8], s: &[u8]) -> Vec<u8> {
		let total_len = 2 + r.len() + 2 + s.len();
		let mut der = Vec::with_capacity(2 + total_len);
		der.push(0x30);
		#[allow(clippy::cast_possible_truncation)] // test values are small
		der.push(total_len as u8);
		der.push(0x02);
		#[allow(clippy::cast_possible_truncation)]
		der.push(r.len() as u8);
		der.extend_from_slice(r);
		der.push(0x02);
		#[allow(clippy::cast_possible_truncation)]
		der.push(s.len() as u8);
		der.extend_from_slice(s);
		der
	}

	#[test]
	fn lenient_der_strict_encoding() {
		// Minimal valid DER: R=1, S=1
		let der = build_der(&[0x01], &[0x01]);
		let sig = parse_der_lenient(&der);
		assert!(sig.is_some());
	}

	#[test]
	fn lenient_der_extra_leading_zeros_in_r() {
		// R has unnecessary leading zeros: [0x00, 0x00, 0x01]
		// Strict DER rejects this, lenient should accept
		let der = build_der(&[0x00, 0x00, 0x01], &[0x01]);
		let sig = parse_der_lenient(&der);
		assert!(sig.is_some());

		// Should produce the same scalar as strict encoding
		let strict = build_der(&[0x01], &[0x01]);
		assert_eq!(parse_der_lenient(&der).unwrap(), parse_der_lenient(&strict).unwrap());
	}

	#[test]
	fn lenient_der_extra_leading_zeros_in_s() {
		let der = build_der(&[0x01], &[0x00, 0x00, 0x01]);
		let sig = parse_der_lenient(&der);
		assert!(sig.is_some());
	}

	#[test]
	fn lenient_der_trailing_garbage() {
		// Strict DER doesn't allow trailing bytes after S
		let mut der = build_der(&[0x01], &[0x01]);
		der.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
		let sig = parse_der_lenient(&der);
		assert!(sig.is_some());

		// Should produce the same signature as without garbage
		let clean = build_der(&[0x01], &[0x01]);
		assert_eq!(parse_der_lenient(&der).unwrap(), parse_der_lenient(&clean).unwrap());
	}

	#[test]
	fn lenient_der_wrong_total_length() {
		// Total length byte says 4 but actual content is 6 bytes
		let mut der = build_der(&[0x01], &[0x01]);
		der[1] = 0x04; // wrong total length (should be 6)
		let sig = parse_der_lenient(&der);
		assert!(sig.is_some());
	}

	#[test]
	fn lenient_der_rejects_missing_sequence_tag() {
		let mut der = build_der(&[0x01], &[0x01]);
		der[0] = 0x31; // wrong tag
		assert!(parse_der_lenient(&der).is_none());
	}

	#[test]
	fn lenient_der_rejects_missing_integer_tag() {
		let mut der = build_der(&[0x01], &[0x01]);
		der[2] = 0x03; // wrong R integer tag
		assert!(parse_der_lenient(&der).is_none());
	}

	#[test]
	fn lenient_der_rejects_empty_input() {
		assert!(parse_der_lenient(&[]).is_none());
	}

	#[test]
	fn lenient_der_rejects_zero_length_r() {
		// R with len=0 is invalid -- no integer value
		let der = vec![0x30, 0x04, 0x02, 0x00, 0x02, 0x01, 0x01];
		assert!(parse_der_lenient(&der).is_none());
	}

	#[test]
	fn lenient_der_rejects_r_exceeding_scalar_size() {
		// R with 33 non-zero bytes exceeds secp256k1 scalar size
		let r = vec![0x01; 33];
		let der = build_der(&r, &[0x01]);
		assert!(parse_der_lenient(&der).is_none());
	}

	#[test]
	fn scalar_from_all_zeros() {
		// All-zero input should produce all-zero scalar (from_scalars
		// will reject zero scalars, making verify_ecdsa return false)
		let result = scalar_from_bytes(&[0x00, 0x00, 0x00]);
		assert!(result.is_some());
		assert_eq!(result.unwrap(), [0u8; 32]);
	}
}
