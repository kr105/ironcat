// SPDX-License-Identifier: Apache-2.0

mod logger_channel;
mod network;
mod nodes;
mod ui;
mod utils;

use anyhow::Result;
use logger_channel::{LogChannel, LogChannelEntry};
use network::listening_start;
use nodes::{insert_node, NodeManager};
use simplelog::{format_description, info, CombinedLogger, ConfigBuilder, LevelFilter};
use std::sync::Arc;
use tokio::sync::mpsc;
use ui::tui::tui_start;

#[tokio::main]
async fn main() -> Result<()> {
	let (log_tx, log_rx) = mpsc::channel::<LogChannelEntry>(100);

	// Setup logger config
	let mut log_config = ConfigBuilder::new();
	let log_config = log_config.set_time_format_custom(format_description!(
		"[hour repr:24]:[minute]:[second].[subsecond digits:3]"
	));

	// Try to set logging time to local timezone
	let log_config = match log_config.set_time_offset_to_local() {
		Ok(local) => local.build(),
		Err(_) => log_config.build(),
	};

	// Init loggers
	CombinedLogger::init(vec![LogChannel::new(LevelFilter::Trace, log_config, log_tx)]).unwrap();

	info!("ironcat v0.0.2 - Starting ...");

	let node_manager = Arc::new(NodeManager::new());

	// Start UI
	let nm_clone = Arc::clone(&node_manager);
	let ui_handle = tokio::spawn(tui_start(nm_clone, log_rx));

	// Start listening thread
	let nm_clone: Arc<NodeManager> = Arc::clone(&node_manager);

	let listening_handle = tokio::spawn(listening_start(nm_clone));

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, "127.0.0.1", 9933);

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, "109.184.54.50", 9933);

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, "23.179.3.12", 9933);

	// Keep the main task running
	tokio::select! {
		_ = tokio::signal::ctrl_c() => {
			println!("Received Ctrl+C, shutting down...");
		}
		_ = ui_handle => {
			println!("UI task ended, shutting down...");
		}
		_ = listening_handle => {
			println!("Listening task ended, shutting down...");
		}
	}

	Ok(())
}
