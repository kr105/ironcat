// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use sha2::{Digest, Sha256};

/// Length of a Hash256 in bytes
pub const HASH_LEN: usize = 32;

/// A 256-bit hash stored in wire order (raw SHA-256 output byte order)
///
/// Display and debug formatting show the hash in reversed hex (block explorer convention),
/// matching how block hashes and txids are typically displayed
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Hash256([u8; HASH_LEN]);

impl Hash256 {
	/// All-zero hash, used as the sentinel for coinbase previous output
	pub const ZERO: Self = Self([0u8; HASH_LEN]);

	/// Creates a `Hash256` from a raw 32-byte array in wire order
	pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
		Self(bytes)
	}

	/// Returns the raw 32 bytes in wire order
	pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
		&self.0
	}
}

impl fmt::Display for Hash256 {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		// Display in reversed byte order (block explorer convention)
		for byte in self.0.iter().rev() {
			write!(f, "{byte:02x}")?;
		}
		Ok(())
	}
}

impl fmt::Debug for Hash256 {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Hash256({self})")
	}
}

/// Computes the double SHA-256 hash of the given data
///
/// This is the standard hash function used throughout Bitcoin-derived protocols
/// for block hashes, transaction IDs, and message checksums
///
/// # Panics
///
/// Cannot panic: SHA-256 always produces exactly 32 bytes
pub fn double_sha256(data: &[u8]) -> Hash256 {
	let mut hasher = Sha256::new();
	hasher.update(data);
	let first = hasher.finalize_reset();
	hasher.update(first);
	let second = hasher.finalize();

	// SHA-256 always produces exactly 32 bytes
	#[allow(clippy::unwrap_used)] // SHA-256 output is always 32 bytes
	let bytes: [u8; HASH_LEN] = second.as_slice().try_into().unwrap();
	Hash256(bytes)
}
