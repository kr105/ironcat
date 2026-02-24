// SPDX-License-Identifier: Apache-2.0

mod cli;
mod network;
mod nodes;
mod tui_layer;
mod ui;
mod utils;

use anyhow::Result;
use clap::Parser;
use cli::Args;
use network::listening_start;
use nodes::{insert_node, NodeManager};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tui_layer::{TuiLayer, TuiLogEntry};
use ui::tui::tui_start;

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();

	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("trace"));

	if args.daemon {
		// Daemon mode: log to stderr with timestamps
		tracing_subscriber::registry()
			.with(env_filter)
			.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
			.init();
	} else {
		// TUI mode: log to channel for display in the terminal UI
		let (log_tx, log_rx) = mpsc::channel::<TuiLogEntry>(100);
		let tui_layer = TuiLayer::new(log_tx);

		tracing_subscriber::registry().with(env_filter).with(tui_layer).init();

		return run_with_tui(log_rx).await;
	}

	info!("ironcat v0.0.2 - Starting in daemon mode");

	run_daemon().await
}

/// Runs ironcat in TUI mode with the terminal UI
async fn run_with_tui(log_rx: mpsc::Receiver<TuiLogEntry>) -> Result<()> {
	info!("ironcat v0.0.2 - Starting ...");

	let node_manager = Arc::new(NodeManager::new());

	// Spawn the TUI on a blocking thread so it doesn't block a tokio worker
	let nm_clone = Arc::clone(&node_manager);
	let ui_handle = tokio::task::spawn_blocking(move || tui_start(nm_clone, log_rx));

	let nm_clone = Arc::clone(&node_manager);
	let listening_handle = tokio::spawn(listening_start(nm_clone));

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, IpAddr::V4(Ipv4Addr::new(161, 129, 176, 92)), 9933);

	tokio::select! {
		_ = tokio::signal::ctrl_c() => {
			info!("Received Ctrl+C, shutting down");
		}
		_ = ui_handle => {
			info!("UI task ended, shutting down");
		}
		_ = listening_handle => {
			info!("Listening task ended, shutting down");
		}
	}

	Ok(())
}

/// Runs ironcat in daemon mode (no TUI, logs to stderr)
async fn run_daemon() -> Result<()> {
	let node_manager = Arc::new(NodeManager::new());

	let nm_clone = Arc::clone(&node_manager);
	let listening_handle = tokio::spawn(listening_start(nm_clone));

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, IpAddr::V4(Ipv4Addr::new(161, 129, 176, 92)), 9933);

	tokio::select! {
		_ = tokio::signal::ctrl_c() => {
			info!("Received Ctrl+C, shutting down");
		}
		_ = listening_handle => {
			info!("Listening task ended, shutting down");
		}
	}

	Ok(())
}
