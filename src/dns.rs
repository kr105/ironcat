// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, net::SocketAddr, time::Duration};

use rand::seq::SliceRandom;
use tokio::time::timeout;
use tracing::{info, warn};

/// DNS seed hostnames for the Catcoin mainnet
const DNS_SEEDS: &[&str] = &[
	"catcoin.seeds.multicoin.co",
	"dnsseed.catalyst.ovh",
	"dnsseed.catcointomars.top",
	"dnsseed.wildcat.ovh",
	"dnsseed.catcoin.ovh",
	"dnsseed.bcats.top",
	"dnsseed.jjcatcoin.top",
	"dnsseed.catsonmylap.top",
	"dnsseed.catcoin.party",
	"dnsseed.remembermeasyoupassby.top",
	"seed.catcoinwallets.com",
];

/// Default port for the Catcoin mainnet P2P protocol
pub const DEFAULT_PORT: u16 = 9933;

/// Timeout applied to each individual DNS resolution attempt
const DNS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves all DNS seeds in parallel and returns a shuffled, deduplicated list of peer addresses
pub async fn resolve_dns_seeds() -> Vec<SocketAddr> {
	let mut join_set = tokio::task::JoinSet::new();

	for &seed in DNS_SEEDS {
		join_set.spawn(async move { resolve_single_seed(seed).await });
	}

	let mut seen = HashSet::new();
	while let Some(result) = join_set.join_next().await {
		match result {
			Ok(addrs) => {
				seen.extend(addrs);
			}
			Err(err) => {
				warn!(error = %err, "DNS seed task panicked");
			}
		}
	}

	let mut addrs: Vec<SocketAddr> = seen.into_iter().collect();
	addrs.shuffle(&mut rand::rng());

	info!(
		count = addrs.len(),
		total_seeds = DNS_SEEDS.len(),
		"DNS seed resolution complete"
	);

	addrs
}

/// Resolves a single DNS seed hostname to a list of socket addresses
///
/// Returns an empty vec on timeout or any resolution failure
async fn resolve_single_seed(seed: &str) -> Vec<SocketAddr> {
	let host_port = format!("{seed}:{DEFAULT_PORT}");

	let result = timeout(DNS_RESOLVE_TIMEOUT, tokio::net::lookup_host(host_port)).await;

	match result {
		Ok(Ok(addrs)) => {
			let collected: Vec<SocketAddr> = addrs.collect();
			info!(seed, count = collected.len(), "Resolved DNS seed");
			collected
		}
		Ok(Err(err)) => {
			warn!(seed, error = %err, "Failed to resolve DNS seed");
			vec![]
		}
		Err(_) => {
			warn!(seed, "DNS seed resolution timed out");
			vec![]
		}
	}
}

#[cfg(test)]
// Tests use unwrap/expect for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;

	#[test]
	fn dns_seeds_list_is_not_empty() {
		assert!(!DNS_SEEDS.is_empty());
	}

	#[test]
	fn dns_seeds_are_hostnames_not_ips() {
		for seed in DNS_SEEDS {
			assert!(
				seed.chars().any(char::is_alphabetic),
				"seed '{seed}' does not look like a hostname"
			);
		}
	}

	#[test]
	fn default_port_is_catcoin_mainnet() {
		assert_eq!(DEFAULT_PORT, 9933);
	}

	#[tokio::test]
	async fn bogus_hostname_returns_empty() {
		let result = resolve_single_seed("this.hostname.does.not.exist.invalid").await;
		assert!(result.is_empty());
	}
}
