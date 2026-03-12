// SPDX-License-Identifier: Apache-2.0

//! Overview tab -- 2x2 grid of stats panels

use ratatui::{
	Frame,
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
};

use crate::difficulty::{self, ConsensusParams};
use crate::mempool::MempoolStats;
use crate::nodes::{NodeSnapshot, NodeStats};

use super::tui::{format_number, format_progress_bar};

/// Data needed to render the overview tab
pub struct OverviewData<'a> {
	/// Network statistics (peer counts by state)
	pub stats: &'a NodeStats,
	/// Snapshot of all peer nodes
	pub nodes: &'a [NodeSnapshot],
	/// Current header chain height
	pub chain_height: u32,
	/// Compact difficulty bits at chain tip
	pub tip_bits: u32,
	/// Current validated block height
	pub block_height: u32,
	/// Mempool statistics snapshot
	pub mempool_stats: &'a MempoolStats,
	/// Seconds since last block was processed
	pub last_block_secs: Option<u64>,
	/// Transaction count in last processed block
	pub last_block_tx_count: Option<usize>,
}

/// Renders the overview tab with a 2x2 stats grid
pub fn render(frame: &mut Frame, area: Rect, data: &OverviewData) {
	let main_layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(7), // top row
			Constraint::Length(7), // bottom row
			Constraint::Min(0),    // absorb remaining space
		])
		.split(area);

	// Top row: Network | Chain Sync
	#[allow(clippy::indexing_slicing)] // layout produces exactly 3 elements
	let top_row = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
		.split(main_layout[0]);

	// Bottom row: Mempool | Chainstate
	#[allow(clippy::indexing_slicing)] // layout produces exactly 3 elements
	let bottom_row = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
		.split(main_layout[1]);

	#[allow(clippy::indexing_slicing)] // layout produces exactly 2 elements
	{
		render_network(frame, top_row[0], data);
		render_chain_sync(frame, top_row[1], data);
		render_mempool(frame, bottom_row[0], data);
		render_chainstate(frame, bottom_row[1], data);
	}
}

/// Renders the Network stats panel
fn render_network(frame: &mut Frame, area: Rect, data: &OverviewData) {
	let text = vec![
		Line::from(vec![
			Span::styled("Peers: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{}", data.stats.connected),
				Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
			),
			Span::styled(
				format!("  ({} in / {} out)", data.stats.incoming, data.stats.outgoing),
				Style::default().fg(Color::DarkGray),
			),
		]),
		Line::from(vec![
			Span::styled("Handshaking: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{}", data.stats.handshaking),
				Style::default().fg(Color::Yellow),
			),
		]),
		Line::from(vec![
			Span::styled("Banned: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", data.stats.banned), Style::default().fg(Color::Red)),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Network")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Renders the Chain Sync panel
fn render_chain_sync(frame: &mut Frame, area: Rect, data: &OverviewData) {
	// Calculate block sync progress (blocks processed / total headers)
	#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // heights fit in f64, division needed for ratio
	let progress = if data.chain_height > 0 {
		let ratio = f64::from(data.block_height) / f64::from(data.chain_height);
		ratio.min(1.0)
	} else {
		0.0
	};

	let text = vec![
		Line::from(vec![
			Span::styled("Blocks: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!(
					"{} / {}",
					format_number(data.block_height),
					format_number(data.chain_height)
				),
				Style::default().fg(if data.block_height >= data.chain_height {
					Color::Green
				} else {
					Color::Yellow
				}),
			),
		]),
		Line::from(vec![
			Span::styled(
				format_progress_bar(progress, 20),
				Style::default().fg(if progress >= 1.0 { Color::Green } else { Color::Yellow }),
			),
			#[allow(clippy::float_arithmetic)] // percentage display requires multiplication
			Span::styled(format!(" {:.1}%", progress * 100.0), Style::default().fg(Color::White)),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Chain Sync")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Renders the Mempool stats panel
fn render_mempool(frame: &mut Frame, area: Rect, data: &OverviewData) {
	#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // mempool size fits in f64, division for MB display
	let size_mb = data.mempool_stats.total_size as f64 / (1024.0 * 1024.0);

	let min_rate = data
		.mempool_stats
		.min_fee_rate
		.map_or_else(|| "-".to_string(), |r| format!("{r:.1} catoshis/B"));

	let text = vec![
		Line::from(vec![
			Span::styled("Transactions: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{}", data.mempool_stats.count),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			),
		]),
		Line::from(vec![
			Span::styled("Size: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{size_mb:.2} / 50 MB"), Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Min fee: ", Style::default().fg(Color::Gray)),
			Span::styled(min_rate, Style::default().fg(Color::White)),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Mempool")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Renders the Chainstate panel
fn render_chainstate(frame: &mut Frame, area: Rect, data: &OverviewData) {
	let last_block_label = match (data.last_block_secs, data.last_block_tx_count) {
		(Some(secs), Some(txs)) => format!("{txs} txs, {secs}s ago"),
		(Some(secs), None) => format!("{secs}s ago"),
		_ => "-".to_string(),
	};

	let diff = difficulty::compact_to_difficulty(data.tip_bits);
	let algo = difficulty::active_algorithm(data.block_height, &ConsensusParams::mainnet());

	let text = vec![
		Line::from(vec![
			Span::styled("Last block: ", Style::default().fg(Color::Gray)),
			Span::styled(last_block_label, Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Difficulty: ", Style::default().fg(Color::Gray)),
			Span::styled(
				difficulty::format_difficulty(diff),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			),
			Span::styled("  ", Style::default()),
			Span::styled(algo, Style::default().fg(Color::Cyan)),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Chainstate")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}
