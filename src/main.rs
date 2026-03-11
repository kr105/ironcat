// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use ironcat::{
	chainstate::ChainState,
	cli::Args,
	difficulty::ConsensusParams,
	dns,
	mempool::Mempool,
	network::{listening_start, SharedTcpWriterExt},
	nodes::{block_download::BlockDownloadManager, NodeManager},
	storage::{
		self, block_store::BlockStore, header_store_backend::HeaderStoreBackend, header_store_redb::RedbHeaderStore,
	},
	tui_layer::{TuiLayer, TuiLogEntry},
	types::{block::BlockHeader, hash::Hash256},
	ui::tui::tui_start,
};

/// Catcoin mainnet genesis block header
///
/// From chainparams.cpp: CreateGenesisBlock(1387838302, 588050, 0x1e0ffff0, 1, 50 * COIN)
/// Hash: bc3b4ec43c4ebb2fef49e6240812549e61ffa623d9418608aa90eaad26c96296
const GENESIS_HEADER: BlockHeader = BlockHeader {
	version: 1,
	prev_hash: Hash256::ZERO,
	merkle_root: Hash256::from_bytes([
		0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb, 0x13, 0x8d, 0x1c, 0x43,
		0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5, 0x3d, 0xa3, 0x07, 0x40,
	]),
	timestamp: 1_387_838_302,
	bits: 0x1e0f_fff0,
	nonce: 588_050,
};

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();

	let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

	// Open debug.log in append mode unless --no-logfile is set.
	// The datadir must be created before opening the log file
	let file_layer = if args.no_logfile {
		None
	} else {
		std::fs::create_dir_all(&args.datadir).ok();
		let log_path = args.datadir.join("debug.log");
		match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
			Ok(file) => Some(
				tracing_subscriber::fmt::layer()
					.with_writer(file)
					.with_ansi(false)
					.with_timer(tracing_subscriber::fmt::time::uptime()),
			),
			Err(e) => {
				// Subscriber is not initialized yet, write directly to stderr
				let _ = std::io::Write::write_fmt(
					&mut std::io::stderr(),
					format_args!("warning: failed to open {}: {e}\n", log_path.display()),
				);
				None
			}
		}
	};

	if args.daemon {
		tracing_subscriber::registry()
			.with(env_filter)
			.with(file_layer)
			.with(
				tracing_subscriber::fmt::layer()
					.with_writer(std::io::stderr)
					.with_timer(tracing_subscriber::fmt::time::uptime()),
			)
			.init();

		info!("ironcat v{} - Starting in daemon mode", env!("CARGO_PKG_VERSION"));
		run_core(None, &args).await
	} else {
		let (log_tx, log_rx) = mpsc::channel::<TuiLogEntry>(100);
		let tui_layer = TuiLayer::new(log_tx);

		tracing_subscriber::registry()
			.with(env_filter)
			.with(file_layer)
			.with(tui_layer)
			.init();

		info!("ironcat v{} - Starting ...", env!("CARGO_PKG_VERSION"));
		run_core(Some(log_rx), &args).await
	}
}

/// Opens the header persistence backend, returning None on failure
fn open_header_backend(datadir: &std::path::Path) -> Option<Arc<dyn HeaderStoreBackend>> {
	let db_path = datadir.join("headers.redb");
	match RedbHeaderStore::open(&db_path) {
		Ok(store) => Some(Arc::new(store)),
		Err(e) => {
			error!(error = %e, "failed to open header database, headers will not persist");
			None
		}
	}
}

/// Core application loop shared between TUI and daemon modes
#[allow(clippy::too_many_lines)] // sequential startup steps, splitting would obscure flow
async fn run_core(tui_rx: Option<mpsc::Receiver<TuiLogEntry>>, args: &Args) -> Result<()> {
	// Ensure data directory exists before opening any databases
	std::fs::create_dir_all(&args.datadir)
		.with_context(|| format!("failed to create data directory {}", args.datadir.display()))?;

	// Create NodeManager without backend first so the TUI can start immediately
	let node_manager = Arc::new(NodeManager::new(GENESIS_HEADER, ConsensusParams::mainnet()));

	// Create mempool early so the TUI can display it from the start
	let mempool = Arc::new(tokio::sync::RwLock::new(Mempool::new()));
	node_manager.set_mempool(Arc::clone(&mempool));

	// Spawn TUI before any heavy I/O so loading progress is visible
	let ui_handle = tui_rx.map(|log_rx| {
		let nm = Arc::clone(&node_manager);
		let mp = Arc::clone(&mempool);
		tokio::task::spawn_blocking(move || tui_start(nm, log_rx, mp))
	});

	// Load headers from backend (slow: reads + hashes all stored headers)
	// Runs on the blocking pool so the TUI can redraw concurrently.
	// Only the final HashMap swap briefly acquires the write lock
	let header_backend = open_header_backend(&args.datadir);
	if let Some(backend) = header_backend {
		let nm = Arc::clone(&node_manager);
		tokio::task::spawn_blocking(move || {
			nm.attach_header_backend(backend);
		})
		.await
		.context("header loading task panicked")?;
	}

	// Open block store for raw block data
	let block_store = Arc::new(BlockStore::open(&args.datadir).context("failed to open block store")?);

	// Open chainstate database for UTXO set management
	// Script verification is skipped for blocks at or below the last checkpoint height
	let consensus = ConsensusParams::mainnet();
	let chainstate = Arc::new(
		ChainState::open(&args.datadir, consensus.last_checkpoint_height()).context("failed to open chainstate")?,
	);

	// Wire chainstate into the node manager for tx validation
	node_manager.set_chainstate(Arc::clone(&chainstate));

	// Create channel for forwarding received blocks to the download manager
	let (block_tx, block_rx) = tokio::sync::mpsc::channel(512);
	node_manager.set_block_sender(block_tx);

	// Create channel for peer disconnect notifications so the download
	// manager can immediately reassign in-flight blocks
	let (disconnect_tx, disconnect_rx) = tokio::sync::mpsc::unbounded_channel();
	node_manager.set_disconnect_sender(disconnect_tx);

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

	// Spawn listener
	let nm = Arc::clone(&node_manager);
	let listen_port = args.port;
	let listener_handle = tokio::spawn(async move {
		if let Err(e) = listening_start(nm, listen_port).await {
			error!("Listener failed: {e:#}");
		}
	});

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

	// Spawn block download manager on blocking pool because the constructor
	// loads all indexed hashes from redb (can take several seconds depending on block count)
	let nm = Arc::clone(&node_manager);
	let bs = Arc::clone(&block_store);
	let cs = Arc::clone(&chainstate);
	let mp = Arc::clone(&mempool);
	let download_handle = tokio::spawn(async move {
		let mut manager = match tokio::task::spawn_blocking({
			let nm = Arc::clone(&nm);
			let bs = Arc::clone(&bs);
			move || BlockDownloadManager::new(nm, bs, block_rx, disconnect_rx, cs, mp)
		})
		.await
		{
			Ok(m) => m,
			Err(e) => {
				error!("block download manager init panicked: {e}");
				return;
			}
		};
		manager.run().await;
	});

	// Spawn tx relay task that announces new mempool txs to peers
	let relay_nm = Arc::clone(&node_manager);
	let relay_mempool = Arc::clone(&mempool);
	let relay_handle = tokio::spawn(async move {
		let mut rx = relay_mempool.read().await.subscribe();
		loop {
			match rx.recv().await {
				Ok(txid) => {
					let inv_item = ironcat::network::message_inv::InvItem {
						inv_type: ironcat::network::message_inv::InvType::Tx,
						hash: txid,
					};
					let inv_msg = ironcat::network::message_inv::MessageInv::new(vec![inv_item]);
					let inv_bytes = inv_msg.to_bytes();

					let writers = relay_nm.get_connected_writers();
					let mut sent_to = Vec::new();

					for (addr, writer) in &writers {
						if relay_nm.peer_knows_inv(addr, &txid) {
							continue;
						}

						if let Err(e) = writer.send_message("inv", &inv_bytes).await {
							tracing::debug!(peer = %addr, error = %e, "failed to relay tx inv");
						} else {
							sent_to.push(addr);
						}
					}

					for addr in &sent_to {
						relay_nm.mark_peer_inv_known(addr, txid);
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					tracing::warn!(skipped = n, "tx relay lagged, some txs not announced");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
			}
		}
	});

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
		_ = download_handle => {
			info!("Download manager ended, shutting down");
		}
		_ = relay_handle => {
			info!("Tx relay task ended, shutting down");
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
