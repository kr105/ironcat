// SPDX-License-Identifier: Apache-2.0

use std::{
	array::TryFromSliceError,
	net::Ipv4Addr,
	time::{SystemTime, UNIX_EPOCH},
};

/// Returns the current Unix timestamp in seconds
///
/// # Panics
/// If the system clock is before 1970, which is not a realistic scenario
pub fn unix_now() -> u64 {
	#[allow(clippy::expect_used)]
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("system clock before UNIX epoch")
		.as_secs()
}

/// Converts a little-endian byte slice to u64
pub fn vec_to_u64_le(v: &[u8]) -> Result<u64, TryFromSliceError> {
	let bytes: [u8; 8] = v.try_into()?;
	Ok(u64::from_le_bytes(bytes))
}

/// Checks if a timestamp is within the last 24 hours (with 10-minute future tolerance)
pub fn is_recently_active(timestamp: u32) -> bool {
	let now = unix_now();

	// Catcoin protocol uses u32 timestamps, valid until 2106
	#[allow(clippy::cast_possible_truncation)]
	let now = now as u32;

	// Allow timestamps up to 10 minutes in the future (clock skew tolerance)
	let max_future = now.saturating_add(60 * 10);

	// Accept nodes seen within the last 24 hours
	let cutoff = now.saturating_sub(60 * 60 * 24);

	timestamp >= cutoff && timestamp <= max_future
}

/// Converts an IPv4 address to an IPv4-mapped IPv6 address in network byte order
pub const fn ipv4_to_mapped_ipv6(ipv4: Ipv4Addr) -> [u8; 16] {
	ipv4.to_ipv6_mapped().octets()
}
