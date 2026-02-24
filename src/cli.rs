// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use std::net::SocketAddr;

/// ironcat - Catcoin network client
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
	/// Run in daemon mode (no TUI, logs to stderr)
	#[arg(long)]
	pub daemon: bool,

	/// Seed node address to connect to on startup
	#[arg(long, default_value = "161.129.176.92:9933")]
	pub seed: SocketAddr,

	/// Disable DNS seed discovery
	#[arg(long)]
	pub no_dns_seed: bool,
}

#[cfg(test)]
// Tests use unwrap for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;

	#[test]
	fn default_args_no_daemon() {
		let args = Args::parse_from(["ironcat"]);
		assert!(!args.daemon);
	}

	#[test]
	fn daemon_flag_sets_true() {
		let args = Args::parse_from(["ironcat", "--daemon"]);
		assert!(args.daemon);
	}

	#[test]
	fn default_seed_address() {
		let args = Args::parse_from(["ironcat"]);
		assert_eq!(args.seed.to_string(), "161.129.176.92:9933");
	}

	#[test]
	fn custom_seed_address() {
		let args = Args::parse_from(["ironcat", "--seed", "10.0.0.1:8080"]);
		assert_eq!(args.seed.to_string(), "10.0.0.1:8080");
	}

	#[test]
	fn default_dns_seed_enabled() {
		let args = Args::parse_from(["ironcat"]);
		assert!(!args.no_dns_seed);
	}

	#[test]
	fn no_dns_seed_flag() {
		let args = Args::parse_from(["ironcat", "--no-dns-seed"]);
		assert!(args.no_dns_seed);
	}
}
