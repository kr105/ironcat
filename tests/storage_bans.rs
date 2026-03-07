// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::difficulty::ConsensusParams;
use ironcat::storage::bans::{BanDb, SavedBan};
use ironcat::storage::{checksummed_decode, checksummed_encode};
use std::net::{IpAddr, Ipv4Addr};

fn sample_ban() -> SavedBan {
	SavedBan {
		ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
		reason: "protocol violation".to_string(),
		created: 1_700_000_000,
		expires: 1_700_086_400,
	}
}

#[test]
fn roundtrip_ban_db() {
	let db = BanDb {
		version: 1,
		bans: vec![sample_ban()],
	};
	let encoded = checksummed_encode(&db).unwrap();
	let decoded: BanDb = checksummed_decode(&encoded).unwrap();
	assert_eq!(decoded.version, 1);
	assert_eq!(decoded.bans.len(), 1);
	assert_eq!(decoded.bans[0].ip, sample_ban().ip);
	assert_eq!(decoded.bans[0].reason, "protocol violation");
	assert_eq!(decoded.bans[0].created, 1_700_000_000);
	assert_eq!(decoded.bans[0].expires, 1_700_086_400);
}

#[test]
fn empty_ban_db_roundtrip() {
	let db = BanDb {
		version: 1,
		bans: vec![],
	};
	let encoded = checksummed_encode(&db).unwrap();
	let decoded: BanDb = checksummed_decode(&encoded).unwrap();
	assert_eq!(decoded.version, 1);
	assert!(decoded.bans.is_empty());
}

#[test]
fn corrupt_ban_checksum_rejected() {
	let db = BanDb {
		version: 1,
		bans: vec![sample_ban()],
	};
	let mut encoded = checksummed_encode(&db).unwrap();
	encoded[0] ^= 0xFF;
	let result: Result<BanDb, _> = checksummed_decode(&encoded);
	assert!(result.is_err());
}

#[test]
fn save_and_load_bans_file() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("banlist.dat");

	let db = BanDb {
		version: 1,
		bans: vec![sample_ban()],
	};

	ironcat::storage::bans::save(&path, &db).unwrap();
	let loaded: BanDb = ironcat::storage::load_file(&path).unwrap();
	assert_eq!(loaded.bans.len(), 1);
	assert_eq!(loaded.bans[0].reason, "protocol violation");
}

#[test]
fn filter_expired_bans() {
	let db = BanDb {
		version: 1,
		bans: vec![
			SavedBan {
				ip: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
				reason: "misbehavior".to_string(),
				created: 0,
				expires: 1, // expired long ago
			},
			SavedBan {
				ip: IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)),
				reason: "protocol violation".to_string(),
				created: u64::MAX / 2,
				expires: u64::MAX, // far future, still active
			},
		],
	};
	let active = db.active_bans();
	assert_eq!(active.len(), 1);
	assert_eq!(active[0].ip, IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)));
}

use ironcat::nodes::NodeManager;
use ironcat::types::{block::BlockHeader, hash::Hash256};
use ironcat::utils::unix_now;

#[test]
fn collect_bans_from_node_manager() {
	let genesis = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 0,
		bits: 0,
		nonce: 0,
	};
	let nm = NodeManager::new(genesis, ConsensusParams::mainnet());
	// Load a ban via the persistence API
	let now = unix_now();
	nm.load_saved_bans(&BanDb {
		version: 1,
		bans: vec![SavedBan {
			ip: IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)),
			reason: "protocol violation".to_string(),
			created: now,
			expires: u64::MAX, // far future so it's active
		}],
	});

	let db = nm.collect_bans_for_save();
	assert_eq!(db.bans.len(), 1);
	assert_eq!(db.bans[0].ip, IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)));
}
