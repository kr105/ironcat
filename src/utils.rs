// SPDX-License-Identifier: Apache-2.0

pub fn vec_to_u64_le(v: Vec<u8>) -> u64 {
    if v.len() != 8 {
        panic!("Vector must contain exactly 8 bytes");
    }
    u64::from_le_bytes(v.try_into().unwrap())
}

pub fn u64_to_vec_le(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}
