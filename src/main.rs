// SPDX-License-Identifier: Apache-2.0

mod network;
mod nodes;
mod utils;

use anyhow::Result;
use nodes::{insert_node, NodeManager};
use simplelog::{
	format_description, info, ColorChoice, CombinedLogger, ConfigBuilder, LevelFilter, TermLogger, TerminalMode,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
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
	CombinedLogger::init(vec![TermLogger::new(
		LevelFilter::Debug,
		log_config,
		TerminalMode::Mixed,
		ColorChoice::Auto,
	)])
	.unwrap();

	info!("ironcat v0.0.1 - Starting ...");

	let node_manager = Arc::new(NodeManager::new());

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, "127.0.0.1", 9933);

	let nm_clone = Arc::clone(&node_manager);
	insert_node(nm_clone, "161.129.176.90", 9933);

	// Keep the main task running
	tokio::signal::ctrl_c().await?;
	Ok(())
}
