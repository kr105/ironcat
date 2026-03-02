// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::storage::peers::{PeerDb, SavedPeer};
use ironcat::storage::{checksummed_decode, checksummed_encode};
use std::net::{IpAddr, Ipv4Addr};

fn sample_peer() -> SavedPeer {
	SavedPeer {
		ip: IpAddr::V4(Ipv4Addr::new(93, 127, 199, 243)),
		port: 9933,
		services: 1025,
		last_seen: 1_700_000_000,
		user_agent: "/CatcoinCore-Beta:2.1.1/".to_string(),
		height: 423_107,
	}
}

#[test]
fn roundtrip_peer_db() {
	let db = PeerDb {
		version: 1,
		peers: vec![sample_peer()],
	};
	let encoded = checksummed_encode(&db).unwrap();
	let decoded: PeerDb = checksummed_decode(&encoded).unwrap();
	assert_eq!(decoded.version, 1);
	assert_eq!(decoded.peers.len(), 1);
	assert_eq!(decoded.peers[0].ip, sample_peer().ip);
	assert_eq!(decoded.peers[0].port, 9933);
	assert_eq!(decoded.peers[0].services, 1025);
	assert_eq!(decoded.peers[0].last_seen, 1_700_000_000);
	assert_eq!(decoded.peers[0].user_agent, "/CatcoinCore-Beta:2.1.1/");
	assert_eq!(decoded.peers[0].height, 423_107);
}

#[test]
fn empty_peer_db_roundtrip() {
	let db = PeerDb {
		version: 1,
		peers: vec![],
	};
	let encoded = checksummed_encode(&db).unwrap();
	let decoded: PeerDb = checksummed_decode(&encoded).unwrap();
	assert_eq!(decoded.version, 1);
	assert!(decoded.peers.is_empty());
}

#[test]
fn multiple_peers_roundtrip() {
	let db = PeerDb {
		version: 1,
		peers: vec![
			sample_peer(),
			SavedPeer {
				ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
				port: 8333,
				services: 0,
				last_seen: 0,
				user_agent: String::new(),
				height: 0,
			},
		],
	};
	let encoded = checksummed_encode(&db).unwrap();
	let decoded: PeerDb = checksummed_decode(&encoded).unwrap();
	assert_eq!(decoded.peers.len(), 2);
	assert_eq!(decoded.peers[1].port, 8333);
}

#[test]
fn corrupt_peer_db_checksum_rejected() {
	let db = PeerDb {
		version: 1,
		peers: vec![sample_peer()],
	};
	let mut encoded = checksummed_encode(&db).unwrap();
	encoded[0] ^= 0xFF;
	let result: Result<PeerDb, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn save_and_load_peers_file() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("peers.dat");

	let db = PeerDb {
		version: 1,
		peers: vec![sample_peer()],
	};

	ironcat::storage::peers::save(&path, &db).unwrap();
	let loaded: PeerDb = ironcat::storage::load_file(&path).unwrap();
	assert_eq!(loaded.peers.len(), 1);
	assert_eq!(loaded.peers[0].port, 9933);
}

#[test]
fn load_missing_file_returns_none() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("nonexistent.dat");
	let result: Option<PeerDb> = ironcat::storage::load_file(&path);
	assert!(result.is_none());
}
