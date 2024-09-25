// SPDX-License-Identifier: Apache-2.0

use std::{
	net::Ipv4Addr,
	time::{SystemTime, UNIX_EPOCH},
};

pub fn vec_to_u64_le(v: Vec<u8>) -> u64 {
	if v.len() != 8 {
		panic!("Vector must contain exactly 8 bytes");
	}
	u64::from_le_bytes(v.try_into().unwrap())
}

pub fn u64_to_vec_le(value: u64) -> Vec<u8> {
	value.to_le_bytes().to_vec()
}

pub fn is_recently_active(timestamp: u32) -> bool {
	// Get current timestamp
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs() as u32;

	// Calculate timestamp from 6 hours ago
	let thirty_minutes_ago = now.saturating_sub(60 * 60 * 6);

	// Compare
	timestamp >= thirty_minutes_ago && timestamp <= now
}

/// Converts an IPv4 address to an IPv4-mapped IPv6 address in network byte order
pub fn ipv4_to_mapped_ipv6(ipv4: Ipv4Addr) -> [u8; 16] {
	let mut bytes = [0u8; 16];
	bytes[10] = 0xff;
	bytes[11] = 0xff;
	bytes[12..].copy_from_slice(&ipv4.octets());
	bytes
}
