// SPDX-License-Identifier: Apache-2.0

//! Benchmark for scrypt `PoW` hash performance

#![allow(
	clippy::print_stdout,
	clippy::float_arithmetic,
	clippy::cast_precision_loss,
	clippy::similar_names,
	clippy::cast_lossless
)]

use ironcat::{
	pow::{pow_hash, pow_hash_crate},
	types::{block::BlockHeader, hash::Hash256},
};
use std::time::Instant;

/// Catcoin mainnet genesis block header
const GENESIS_HEADER: BlockHeader = BlockHeader {
	version: 1,
	prev_hash: Hash256::ZERO,
	merkle_root: Hash256::from_bytes([
		0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb, 0x13, 0x8d, 0x1c, 0x43,
		0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5, 0x3d, 0xa3, 0x07, 0x40,
	]),
	timestamp: 1_387_838_302,
	bits: 0x1e0f_fff0,
	nonce: 588_050,
};

fn bench_impl(name: &str, iterations: u32, f: impl Fn(&BlockHeader) -> [u8; 32]) {
	// Warmup
	for _ in 0..10 {
		let _ = f(&GENESIS_HEADER);
	}

	let start = Instant::now();
	for _ in 0..iterations {
		let _ = f(&GENESIS_HEADER);
	}
	let elapsed = start.elapsed();

	let per_hash_us = elapsed.as_micros() as f64 / f64::from(iterations);
	let per_hash_ms = per_hash_us / 1000.0;
	let hashes_per_sec = 1_000_000.0 / per_hash_us;

	println!("  {name}:");
	println!("    Iterations:    {iterations}");
	println!("    Total time:    {:.2} s", elapsed.as_secs_f64());
	println!("    Per hash:      {per_hash_ms:.3} ms ({per_hash_us:.0} us)");
	println!("    Throughput:    {hashes_per_sec:.0} hashes/s");
}

fn main() {
	let iterations = 1000;

	println!("=== scrypt(1024,1,1) PoW hash benchmark ===\n");

	bench_impl("scrypt crate (reference)", iterations, pow_hash_crate);
	println!();
	bench_impl("native (thread-local scratchpad)", iterations, pow_hash);

	println!();

	// Estimate IBD time using native implementation
	// Warmup + measure
	for _ in 0..10 {
		let _ = pow_hash(&GENESIS_HEADER);
	}
	let start = Instant::now();
	for _ in 0..iterations {
		let _ = pow_hash(&GENESIS_HEADER);
	}
	let per_hash_us = start.elapsed().as_micros() as f64 / f64::from(iterations);

	println!("IBD estimates (native):");
	for chain_size in [50_000, 100_000, 200_000, 400_000, 500_000] {
		let est_secs = f64::from(chain_size) * per_hash_us / 1_000_000.0;
		println!(
			"  {chain_size:>7} headers: {:.1} s ({:.1} min)",
			est_secs,
			est_secs / 60.0
		);
	}
}
