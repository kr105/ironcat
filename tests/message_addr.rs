// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::net::{IpAddr, Ipv4Addr};

use ironcat::network::{
	NetworkAddress,
	message_addr::{AddrEntry, MessageAddr},
};

#[test]
fn roundtrip_single_addr() {
	let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9933);
	let msg = MessageAddr::new(vec![addr]);
	let bytes = msg.to_bytes();
	let decoded = MessageAddr::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.entries().len(), 1);
	assert_eq!(decoded.entries()[0].address.port, 9933);
}

#[test]
fn roundtrip_multiple_addrs() {
	let addrs: Vec<NetworkAddress> = (1..=5)
		.map(|i| NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)), 9933))
		.collect();
	let msg = MessageAddr::new(addrs);
	let bytes = msg.to_bytes();
	let decoded = MessageAddr::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.entries().len(), 5);
}

#[test]
fn from_bytes_rejects_count_over_1000() {
	let mut bytes = vec![0xFD];
	bytes.extend_from_slice(&1001u16.to_le_bytes());
	let result = MessageAddr::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn empty_addr_list_produces_varint_zero() {
	let msg = MessageAddr::new(Vec::new());
	assert_eq!(msg.to_bytes(), vec![0x00]);
}

#[test]
fn addresses_preserve_ports() {
	let addrs = vec![
		NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933),
		NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 8080),
	];
	let msg = MessageAddr::new(addrs);
	let bytes = msg.to_bytes();
	let decoded = MessageAddr::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.entries()[0].address.port, 9933);
	assert_eq!(decoded.entries()[1].address.port, 8080);
}

#[test]
fn from_entries_preserves_timestamps() {
	let entries = vec![
		AddrEntry {
			timestamp: 1000,
			address: NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9933),
		},
		AddrEntry {
			timestamp: 2000,
			address: NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 9933),
		},
	];
	let msg = MessageAddr::from_entries(entries);
	let bytes = msg.to_bytes();
	let decoded = MessageAddr::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.entries()[0].timestamp, 1000);
	assert_eq!(decoded.entries()[1].timestamp, 2000);
}
