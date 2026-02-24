// SPDX-License-Identifier: Apache-2.0

use std::{
	array::TryFromSliceError,
	net::Ipv4Addr,
	time::{SystemTime, UNIX_EPOCH},
};

pub fn vec_to_u64_le(v: &[u8]) -> Result<u64, TryFromSliceError> {
	let bytes: [u8; 8] = v.try_into()?;
	Ok(u64::from_le_bytes(bytes))
}

pub fn u64_to_vec_le(value: u64) -> Vec<u8> {
	value.to_le_bytes().to_vec()
}

pub fn is_recently_active(timestamp: u32) -> bool {
	// SystemTime::now().duration_since(UNIX_EPOCH) only fails if system clock
	// is before 1970, which is not a realistic scenario
	#[allow(clippy::expect_used)]
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs();

	// Timestamps in the Bitcoin protocol use u32, which is valid until 2106
	#[allow(clippy::cast_possible_truncation)]
	let now = now as u32;

	// Calculate timestamp from 6 hours ago
	#[allow(clippy::arithmetic_side_effects)]
	let six_hours_ago = now.saturating_sub(60 * 60 * 6);

	// Compare
	timestamp >= six_hours_ago && timestamp <= now
}

/// Converts an IPv4 address to an IPv4-mapped IPv6 address in network byte order
pub fn ipv4_to_mapped_ipv6(ipv4: Ipv4Addr) -> [u8; 16] {
	let mut bytes = [0u8; 16];
	bytes[10] = 0xff;
	bytes[11] = 0xff;
	bytes[12..].copy_from_slice(&ipv4.octets());
	bytes
}
