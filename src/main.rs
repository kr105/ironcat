// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::Parser;
use ironcat::{
	cli::Args,
	dns,
	network::listening_start,
	nodes::NodeManager,
	storage,
	tui_layer::{TuiLayer, TuiLogEntry},
	ui::tui::tui_start,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();

	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

	if args.daemon {
		tracing_subscriber::registry()
			.with(env_filter)
			.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
			.init();

		info!("ironcat v0.0.6 - Starting in daemon mode");
		run_core(None, &args).await
	} else {
		let (log_tx, log_rx) = mpsc::channel::<TuiLogEntry>(100);
		let tui_layer = TuiLayer::new(log_tx);

		tracing_subscriber::registry().with(env_filter).with(tui_layer).init();

		info!("ironcat v0.0.6 - Starting ...");
		run_core(Some(log_rx), &args).await
	}
}

/// Core application loop shared between TUI and daemon modes
async fn run_core(tui_rx: Option<mpsc::Receiver<TuiLogEntry>>, args: &Args) -> Result<()> {
	let node_manager = Arc::new(NodeManager::new());

	// Ensure data directory exists
	std::fs::create_dir_all(&args.datadir)
		.with_context(|| format!("failed to create data directory {}", args.datadir.display()))?;

	// Load bans first so banned IPs get rejected when loading peers
	if let Some(ban_db) = storage::load_file::<storage::bans::BanDb>(&args.datadir.join("banlist.dat")) {
		info!(count = ban_db.bans.len(), "Loaded banlist.dat");
		node_manager.load_saved_bans(&ban_db);
	}

	// Load saved peers
	let loaded_peers =
		if let Some(peer_db) = storage::load_file::<storage::peers::PeerDb>(&args.datadir.join("peers.dat")) {
			let count = peer_db.peers.len();
			if count > 0 {
				info!(count, "Loaded peers.dat");
				node_manager.load_saved_peers(&peer_db);
			}
			count
		} else {
			0
		};

	// Spawn TUI if in TUI mode
	let ui_handle = tui_rx.map(|log_rx| {
		let nm = Arc::clone(&node_manager);
		tokio::task::spawn_blocking(move || tui_start(nm, log_rx))
	});

	// Spawn listener
	let nm = Arc::clone(&node_manager);
	let listener_handle = tokio::spawn(listening_start(nm));

	// Spawn reaper for reconnection
	let nm = Arc::clone(&node_manager);
	let reaper_handle = tokio::spawn(nm.run_reaper());

	// Spawn self-announcement task
	let nm = Arc::clone(&node_manager);
	let announce_handle = tokio::spawn(nm.run_self_announce());

	// Spawn periodic persistence task
	let nm = Arc::clone(&node_manager);
	let datadir = args.datadir.clone();
	let persistence_handle = tokio::spawn(storage::persistence_task(nm, datadir));

	// Peer discovery: if we have saved peers, try them first and only
	// fall back to DNS seeds if none connect within 30 seconds
	if loaded_peers > 0 {
		let nm = Arc::clone(&node_manager);
		let seed = args.seed;
		let no_dns = args.no_dns_seed;
		tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_secs(30)).await;
			if nm.connected_count() > 0 {
				return;
			}
			info!("No saved peers connected after 30s, falling back to DNS seeds");
			if !no_dns {
				let dns_addrs = dns::resolve_dns_seeds().await;
				for addr in &dns_addrs {
					nm.insert_outgoing(addr.ip(), addr.port());
				}
			}
			nm.insert_outgoing(seed.ip(), seed.port());
		});
	} else {
		if !args.no_dns_seed {
			let dns_addrs = dns::resolve_dns_seeds().await;
			for addr in &dns_addrs {
				node_manager.insert_outgoing(addr.ip(), addr.port());
			}
		}
		node_manager.insert_outgoing(args.seed.ip(), args.seed.port());
	}

	tokio::select! {
		_ = tokio::signal::ctrl_c() => {
			info!("Received Ctrl+C, shutting down");
		}
		_ = listener_handle => {
			info!("Listening task ended, shutting down");
		}
		_ = reaper_handle => {
			info!("Reaper task ended, shutting down");
		}
		_ = announce_handle => {
			info!("Self-announce task ended, shutting down");
		}
		_ = persistence_handle => {
			info!("Persistence task ended, shutting down");
		}
		() = async {
			match ui_handle {
				Some(handle) => { let _ = handle.await; }
				None => std::future::pending::<()>().await,
			}
		} => {
			info!("UI task ended, shutting down");
		}
	}

	// Save state before shutting down
	storage::save_all(&node_manager, &args.datadir);

	node_manager.graceful_shutdown().await;

	Ok(())
}
