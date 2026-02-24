// SPDX-License-Identifier: Apache-2.0

use clap::Parser;

/// ironcat - Catcoin network client
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
	/// Run in daemon mode (no TUI, logs to stderr)
	#[arg(long)]
	pub daemon: bool,
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
}
