// SPDX-License-Identifier: Apache-2.0

mod cli;
mod dns;
mod network;
mod nodes;
mod tui_layer;
mod ui;
mod utils;

use anyhow::Result;
use clap::Parser;
use cli::Args;
use network::listening_start;
use nodes::NodeManager;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tui_layer::{TuiLayer, TuiLogEntry};
use ui::tui::tui_start;

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();

	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

	if args.daemon {
		tracing_subscriber::registry()
			.with(env_filter)
			.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
			.init();

		info!("ironcat v0.0.2 - Starting in daemon mode");
		run_core(None, &args).await
	} else {
		let (log_tx, log_rx) = mpsc::channel::<TuiLogEntry>(100);
		let tui_layer = TuiLayer::new(log_tx);

		tracing_subscriber::registry().with(env_filter).with(tui_layer).init();

		info!("ironcat v0.0.2 - Starting ...");
		run_core(Some(log_rx), &args).await
	}
}

/// Core application loop shared between TUI and daemon modes
async fn run_core(tui_rx: Option<mpsc::Receiver<TuiLogEntry>>, args: &Args) -> Result<()> {
	let node_manager = Arc::new(NodeManager::new());

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

	// Discover peers via DNS seeds
	if !args.no_dns_seed {
		let dns_addrs = dns::resolve_dns_seeds().await;
		for addr in &dns_addrs {
			node_manager.insert_outgoing(addr.ip(), addr.port());
		}
	}

	// Always connect to --seed as fallback
	node_manager.insert_outgoing(args.seed.ip(), args.seed.port());

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
		() = async {
			match ui_handle {
				Some(handle) => { let _ = handle.await; }
				None => std::future::pending::<()>().await,
			}
		} => {
			info!("UI task ended, shutting down");
		}
	}

	node_manager.graceful_shutdown().await;

	Ok(())
}
