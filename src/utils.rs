// SPDX-License-Identifier: Apache-2.0

use std::{
	array::TryFromSliceError,
	net::{IpAddr, Ipv4Addr},
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

/// Checks if an IP address is publicly routable (not private, loopback, or reserved)
pub fn is_routable(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => is_routable_v4(v4),
		IpAddr::V6(v6) => {
			if v6.is_loopback() || v6.is_unspecified() {
				return false;
			}
			v6.to_ipv4_mapped().map_or_else(
				|| {
					let segments = v6.segments();
					// Not link-local (fe80::/10)
					(segments[0] & 0xFFC0) != 0xFE80
					// Not unique local (fc00::/7)
					&& (segments[0] & 0xFE00) != 0xFC00
				},
				is_routable_v4,
			)
		}
	}
}

/// Checks if an IPv4 address is publicly routable
const fn is_routable_v4(v4: std::net::Ipv4Addr) -> bool {
	if v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() {
		return false;
	}

	let o = v4.octets();

	// 100.64.0.0/10 (shared address space, RFC 6598)
	if o[0] == 100 && (o[1] & 0xC0) == 64 {
		return false;
	}
	// 192.0.0.0/24 (IETF protocol assignments)
	if o[0] == 192 && o[1] == 0 && o[2] == 0 {
		return false;
	}
	// 198.18.0.0/15 (benchmarking)
	if o[0] == 198 && (o[1] & 0xFE) == 18 {
		return false;
	}
	// 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 (documentation)
	if o[0] == 192 && o[1] == 0 && o[2] == 2 {
		return false;
	}
	if o[0] == 198 && o[1] == 51 && o[2] == 100 {
		return false;
	}
	if o[0] == 203 && o[1] == 0 && o[2] == 113 {
		return false;
	}

	true
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn is_routable_rejects_private() {
		assert!(!is_routable("10.0.0.1".parse().unwrap()));
		assert!(!is_routable("172.16.0.1".parse().unwrap()));
		assert!(!is_routable("192.168.1.1".parse().unwrap()));
	}

	#[test]
	fn is_routable_rejects_loopback() {
		assert!(!is_routable("127.0.0.1".parse().unwrap()));
		assert!(!is_routable("::1".parse().unwrap()));
	}

	#[test]
	fn is_routable_rejects_link_local() {
		assert!(!is_routable("169.254.1.1".parse().unwrap()));
	}

	#[test]
	fn is_routable_accepts_public() {
		assert!(is_routable("8.8.8.8".parse().unwrap()));
		assert!(is_routable("1.1.1.1".parse().unwrap()));
	}

	#[test]
	fn is_routable_rejects_unspecified() {
		assert!(!is_routable("0.0.0.0".parse().unwrap()));
		assert!(!is_routable("::".parse().unwrap()));
	}
}
