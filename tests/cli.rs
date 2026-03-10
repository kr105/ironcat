// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used)]

use clap::Parser;
use ironcat::cli::Args;

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

#[test]
fn default_datadir() {
	let args = Args::parse_from(["ironcat"]);
	let expected = dirs::home_dir().unwrap().join(".ironcat");
	assert_eq!(args.datadir, expected);
}

#[test]
fn custom_datadir() {
	let args = Args::parse_from(["ironcat", "--datadir", "/tmp/test-ironcat"]);
	assert_eq!(args.datadir, std::path::PathBuf::from("/tmp/test-ironcat"));
}

#[test]
fn default_port() {
	let args = Args::parse_from(["ironcat"]);
	assert_eq!(args.port, 9933);
}

#[test]
fn custom_port() {
	let args = Args::parse_from(["ironcat", "--port", "8080"]);
	assert_eq!(args.port, 8080);
}

#[test]
fn default_logfile_enabled() {
	let args = Args::parse_from(["ironcat"]);
	assert!(!args.no_logfile);
}

#[test]
fn no_logfile_flag() {
	let args = Args::parse_from(["ironcat", "--no-logfile"]);
	assert!(args.no_logfile);
}
