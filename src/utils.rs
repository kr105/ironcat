// SPDX-License-Identifier: Apache-2.0

use std::time::{SystemTime, UNIX_EPOCH};

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

    // Calculate timestamp from 3 hours ago
    let thirty_minutes_ago = now.saturating_sub(60 * 60 * 3);

    // Compare
    timestamp >= thirty_minutes_ago && timestamp <= now
}
