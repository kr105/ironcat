// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing/arithmetic for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::arithmetic_side_effects,
	clippy::similar_names,
	clippy::panic,
	clippy::missing_const_for_fn
)]

use std::sync::Arc;

use ironcat::chainstate::ChainState;
use ironcat::chainstate::coin::Coin;
use ironcat::difficulty::ConsensusParams;
use ironcat::headers::HeaderStore;
use ironcat::mempool::Mempool;
use ironcat::network::write_varint;
use ironcat::reorg::{self, ActivateResult};
use ironcat::storage::block_store::BlockStore;
use ironcat::types::block::{Block, BlockHeader};
use ironcat::types::hash::{Hash256, double_sha256};
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};
use ironcat::validation::subsidy::COIN;
use redb::TableDefinition;

/// Test genesis block header (not real Catcoin genesis, just for test isolation)
const GENESIS_HEADER: BlockHeader = BlockHeader {
	version: 1,
	prev_hash: Hash256::ZERO,
	merkle_root: Hash256::ZERO,
	timestamp: 1_387_838_302,
	bits: 0x1e0f_fff0,
	nonce: 42,
};

/// Shared constants for test block headers. All helpers must use
/// identical values so that header hashes match between `make_block`
/// and `make_header`
const TEST_BITS: u32 = 0x1e0f_fff0;
const TEST_TIMESTAMP: u32 = 1_700_000_000;

/// Serializes a block to wire-format bytes (header + varint tx count + txs)
fn serialize_block(block: &Block) -> Vec<u8> {
	let mut bytes = block.header.to_bytes();
	write_varint(&mut bytes, block.transactions.len() as u64);
	for tx in &block.transactions {
		bytes.extend_from_slice(&tx.to_bytes());
	}
	bytes
}

fn block_hash(block: &Block) -> Hash256 {
	double_sha256(&block.header.to_bytes())
}

fn make_coinbase(height: u32, value: i64) -> Transaction {
	let mut script_sig = Vec::new();
	if height == 0 {
		script_sig.push(1);
		script_sig.push(0);
	} else if height <= 0xFF {
		script_sig.push(1);
		script_sig.push(height as u8);
	} else if height <= 0xFFFF {
		script_sig.push(2);
		script_sig.push(height as u8);
		script_sig.push((height >> 8) as u8);
	} else {
		script_sig.push(3);
		script_sig.push(height as u8);
		script_sig.push((height >> 8) as u8);
		script_sig.push((height >> 16) as u8);
	}

	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig,
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9, 0x14],
		}],
		locktime: 0,
	}
}

fn make_block(prev_hash: Hash256, nonce: u32, txs: Vec<Transaction>) -> Block {
	Block {
		header: BlockHeader {
			version: 1,
			prev_hash,
			merkle_root: Hash256::ZERO,
			timestamp: TEST_TIMESTAMP,
			bits: TEST_BITS,
			nonce,
		},
		transactions: txs,
	}
}

/// Creates a simple test transaction spending the given inputs
fn make_tx(inputs: Vec<OutPoint>, outputs: Vec<i64>) -> Transaction {
	Transaction {
		version: 1,
		vin: inputs
			.into_iter()
			.map(|prev_output| TxIn {
				prev_output,
				script_sig: vec![0x01, 0x01],
				sequence: 0xFFFF_FFFF,
			})
			.collect(),
		vout: outputs
			.into_iter()
			.map(|value| TxOut {
				value,
				script_pubkey: vec![0x51], // OP_TRUE
			})
			.collect(),
		locktime: 0,
	}
}

/// Inserts a UTXO directly into the chainstate database for testing
fn seed_utxo(cs: &ChainState, outpoint: &OutPoint, value: i64) {
	const UTXO_SET: TableDefinition<&[u8; 36], &[u8]> = TableDefinition::new("utxos");

	let coin = Coin {
		tx_out: TxOut {
			value,
			script_pubkey: vec![0x51],
		},
		height: 1,
		is_coinbase: false,
	};

	let mut key = [0u8; 36];
	key[..32].copy_from_slice(outpoint.txid.as_bytes());
	key[32..].copy_from_slice(&outpoint.index.to_le_bytes());

	let txn = cs.database().begin_write().unwrap();
	{
		let mut table = txn.open_table(UTXO_SET).unwrap();
		table.insert(&key, coin.to_bytes().as_slice()).unwrap();
	}
	txn.commit().unwrap();
}

/// Full test context with all components wired up
struct ReorgTestCtx {
	chainstate: Arc<ChainState>,
	block_store: Arc<BlockStore>,
	headers: HeaderStore,
	mempool: Mempool,
	_dir: tempfile::TempDir,
}

impl ReorgTestCtx {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let chainstate = Arc::new(ChainState::open(dir.path(), u32::MAX).unwrap());
		let block_store = Arc::new(BlockStore::open(dir.path()).unwrap());
		let headers = HeaderStore::new(GENESIS_HEADER, ConsensusParams::testing());
		let mempool = Mempool::new();
		Self {
			chainstate,
			block_store,
			headers,
			mempool,
			_dir: dir,
		}
	}

	/// Builds a block, connects it to chainstate, stores raw bytes,
	/// adds header to header store, returns (hash, block)
	fn connect_block(&mut self, prev_hash: Hash256, height: u32, nonce: u32) -> (Hash256, Block) {
		let coinbase = make_coinbase(height, 50 * COIN);
		let block = make_block(prev_hash, nonce, vec![coinbase]);
		let hash = block_hash(&block);

		self.chainstate.connect_block(&block, height, &hash).unwrap();

		let raw = serialize_block(&block);
		self.block_store.store_block(&hash, &raw).unwrap();
		self.block_store.flush().unwrap();

		self.headers.add_header(&block.header).unwrap();

		(hash, block)
	}

	/// Adds a fork block: stores raw bytes in block store AND adds
	/// the header to header store via `accept_header`. Does NOT connect
	/// to chainstate. Returns (hash, block)
	fn add_fork_block(&mut self, prev_hash: Hash256, height: u32, nonce: u32) -> (Hash256, Block) {
		let coinbase = make_coinbase(height, 50 * COIN);
		let block = make_block(prev_hash, nonce, vec![coinbase]);
		let hash = block_hash(&block);

		let raw = serialize_block(&block);
		self.block_store.store_block(&hash, &raw).unwrap();
		self.block_store.flush().unwrap();

		self.headers.accept_header(block.header.clone()).unwrap();

		(hash, block)
	}

	/// Adds a fork header only (no block data in block store).
	/// Used for testing the `NeedBlocks` path
	fn add_fork_header(&mut self, prev_hash: Hash256, nonce: u32) -> Hash256 {
		let block = make_block(prev_hash, nonce, vec![]);
		let hash = block_hash(&block);
		self.headers.accept_header(block.header).unwrap();
		hash
	}

	/// Adds a fork block with custom transactions (coinbase + extras).
	/// Stores raw bytes and adds header via `accept_header`
	fn add_fork_block_with_txs(
		&mut self,
		prev_hash: Hash256,
		height: u32,
		nonce: u32,
		extra_txs: Vec<Transaction>,
	) -> (Hash256, Block) {
		let mut txs = vec![make_coinbase(height, 50 * COIN)];
		txs.extend(extra_txs);
		let block = make_block(prev_hash, nonce, txs);
		let hash = block_hash(&block);

		let raw = serialize_block(&block);
		self.block_store.store_block(&hash, &raw).unwrap();
		self.block_store.flush().unwrap();

		self.headers.accept_header(block.header.clone()).unwrap();

		(hash, block)
	}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn activate_best_chain_already_best() {
	let mut ctx = ReorgTestCtx::new();
	let genesis_hash = GENESIS_HEADER.block_hash();

	// Build a 3-block chain (active)
	let (h1, _) = ctx.connect_block(genesis_hash, 1, 100);
	let (h2, _) = ctx.connect_block(h1, 2, 200);
	let (_h3, _) = ctx.connect_block(h2, 3, 300);

	// No fork exists, chainstate tip == header best tip
	let result =
		reorg::activate_best_chain(&mut ctx.headers, &ctx.chainstate, &ctx.block_store, &mut ctx.mempool).unwrap();
	assert!(matches!(result, ActivateResult::AlreadyBest));
}

#[test]
fn activate_best_chain_reorganizes_to_longer_fork() {
	let mut ctx = ReorgTestCtx::new();
	let genesis_hash = GENESIS_HEADER.block_hash();

	// Build a 2-block active chain (heights 1-2)
	let (h1, _) = ctx.connect_block(genesis_hash, 1, 100);
	let (_h2, _) = ctx.connect_block(h1, 2, 200);

	// Build a 3-block fork off genesis (heights 1-3, more work)
	let (f1, _) = ctx.add_fork_block(genesis_hash, 1, 1001);
	let (f2, _) = ctx.add_fork_block(f1, 2, 1002);
	let (f3, _) = ctx.add_fork_block(f2, 3, 1003);

	// Verify fork has more work (longer chain = more cumulative work
	// since all blocks share the same difficulty target)
	let (best_hash, _, _) = ctx.headers.best_tip();
	assert_eq!(best_hash, f3);

	// Run reorg
	let result =
		reorg::activate_best_chain(&mut ctx.headers, &ctx.chainstate, &ctx.block_store, &mut ctx.mempool).unwrap();

	match result {
		ActivateResult::Reorganized {
			old_tip: _,
			new_tip,
			disconnected,
			connected,
		} => {
			assert_eq!(disconnected, 2);
			assert_eq!(connected, 3);
			assert_eq!(new_tip, f3);
		}
		other => panic!("expected Reorganized, got {other:?}"),
	}

	// Chainstate tip should now be the fork tip
	assert_eq!(ctx.chainstate.tip(), f3);
	assert_eq!(ctx.chainstate.tip_height(), 3);
}

#[test]
fn activate_best_chain_need_blocks_when_fork_blocks_missing() {
	let mut ctx = ReorgTestCtx::new();
	let genesis_hash = GENESIS_HEADER.block_hash();

	// Build a 2-block active chain
	let (h1, _) = ctx.connect_block(genesis_hash, 1, 100);
	let (_h2, _) = ctx.connect_block(h1, 2, 200);

	// Add fork headers but do NOT store the fork block data
	let f1 = ctx.add_fork_header(genesis_hash, 2001);
	let f2 = ctx.add_fork_header(f1, 2002);
	let f3 = ctx.add_fork_header(f2, 2003);

	// Reorg should report NeedBlocks
	let result =
		reorg::activate_best_chain(&mut ctx.headers, &ctx.chainstate, &ctx.block_store, &mut ctx.mempool).unwrap();

	match result {
		ActivateResult::NeedBlocks(missing) => {
			assert_eq!(missing.len(), 3);
			assert!(missing.contains(&f1));
			assert!(missing.contains(&f2));
			assert!(missing.contains(&f3));
		}
		other => panic!("expected NeedBlocks, got {other:?}"),
	}

	// Chainstate should be unchanged
	assert_eq!(ctx.chainstate.tip_height(), 2);
}

#[test]
fn activate_best_chain_rejects_deep_reorg() {
	let mut ctx = ReorgTestCtx::new();
	let genesis_hash = GENESIS_HEADER.block_hash();

	// Build a 2-block active chain
	let (h1, _) = ctx.connect_block(genesis_hash, 1, 100);
	let (_h2, _) = ctx.connect_block(h1, 2, 200);

	// Build a fork with 1002 blocks off genesis (exceeds MAX_REORG_DEPTH
	// on the connect side)
	let mut prev = genesis_hash;
	for i in 1..=1002 {
		let (hash, _) = ctx.add_fork_block(prev, i, 10_000 + i);
		prev = hash;
	}

	// The fork has 1002 blocks to connect, exceeding MAX_REORG_DEPTH (1000)
	let result = reorg::activate_best_chain(&mut ctx.headers, &ctx.chainstate, &ctx.block_store, &mut ctx.mempool);

	assert!(result.is_err());
	let err_msg = result.unwrap_err().to_string();
	assert!(
		err_msg.contains("connect depth") && err_msg.contains("exceeds maximum"),
		"expected connect depth error, got: {err_msg}"
	);

	// Chainstate should be unchanged
	assert_eq!(ctx.chainstate.tip_height(), 2);
}

#[test]
fn activate_best_chain_rolls_back_on_invalid_fork_block() {
	let mut ctx = ReorgTestCtx::new();
	let genesis_hash = GENESIS_HEADER.block_hash();

	// Build a 2-block active chain
	let (h1, _) = ctx.connect_block(genesis_hash, 1, 100);
	let (h2, _) = ctx.connect_block(h1, 2, 200);
	let original_tip = h2;

	// Build a 3-block fork off genesis. First two are valid (coinbase only),
	// third contains a tx spending a non-existent UTXO
	let (f1, _) = ctx.add_fork_block(genesis_hash, 1, 5001);
	let (f2, _) = ctx.add_fork_block(f1, 2, 5002);

	// Invalid tx: spends an outpoint that doesn't exist in the UTXO set
	let bad_tx = make_tx(
		vec![OutPoint {
			txid: Hash256::from_bytes([0xDE; 32]),
			index: 0,
		}],
		vec![1_000],
	);
	let (f3, _) = ctx.add_fork_block_with_txs(f2, 3, 5003, vec![bad_tx]);

	// Fork has more work
	let (best_hash, _, _) = ctx.headers.best_tip();
	assert_eq!(best_hash, f3);

	// Reorg should fail: f1 and f2 connect, f3 fails, rollback kicks in
	let result = reorg::activate_best_chain(&mut ctx.headers, &ctx.chainstate, &ctx.block_store, &mut ctx.mempool);

	assert!(result.is_err());
	let err_msg = result.unwrap_err().to_string();
	assert!(
		err_msg.contains("missing utxo"),
		"expected missing utxo error, got: {err_msg}"
	);

	// Chainstate must be back on the original chain after rollback
	assert_eq!(ctx.chainstate.tip(), original_tip);
	assert_eq!(ctx.chainstate.tip_height(), 2);

	// Fork tip should be marked invalid and best_tip should revert to original chain.
	// mark_invalid removes f3 from valid tips and recompute_best_tip selects
	// the active chain tip, which rollback restored to original_tip
	let (best_after, _, _) = ctx.headers.best_tip();
	assert_eq!(
		best_after, original_tip,
		"best tip should revert to original chain after failed reorg"
	);
}

#[test]
fn readd_disconnected_txs_handles_dependencies() {
	let dir = tempfile::tempdir().unwrap();
	let cs = Arc::new(ChainState::open(dir.path(), u32::MAX).unwrap());
	let mut mempool = Mempool::new();

	// Seed a UTXO that tx_a will spend
	let seed_outpoint = OutPoint {
		txid: Hash256::from_bytes([2u8; 32]),
		index: 0,
	};
	seed_utxo(&cs, &seed_outpoint, 100_000);

	// tx_a spends the seeded UTXO
	let tx_a = make_tx(vec![seed_outpoint], vec![90_000]);
	let tx_a_id = tx_a.txid();

	// tx_b spends tx_a's first output (dependency chain)
	let tx_b_outpoint = OutPoint {
		txid: tx_a_id,
		index: 0,
	};
	let tx_b = make_tx(vec![tx_b_outpoint], vec![80_000]);
	let tx_b_id = tx_b.txid();

	// Pass them in reverse dependency order: [tx_b, tx_a]
	// First pass: tx_b fails (input not in UTXO set or mempool),
	// tx_a succeeds. Second pass: tx_b succeeds (tx_a output now
	// available via mempool)
	let readded = mempool.readd_disconnected_txs(vec![tx_b, tx_a], &cs);
	assert_eq!(readded, 2);
	assert!(mempool.contains(&tx_a_id));
	assert!(mempool.contains(&tx_b_id));
}
