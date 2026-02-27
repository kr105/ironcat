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
