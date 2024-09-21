// SPDX-License-Identifier: Apache-2.0

mod network;
mod nodes;

use anyhow::Result;
use nodes::{insert_node, NodeManager};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let node_manager = Arc::new(NodeManager::new());

    let nm_clone = Arc::clone(&node_manager);
    insert_node(nm_clone, "127.0.0.1", 9933).await;

    let nm_clone = Arc::clone(&node_manager);
    //insert_node(nm_clone, "161.129.176.90", 9933).await;

    // Keep the main task running
    tokio::signal::ctrl_c().await?;
    Ok(())
}
