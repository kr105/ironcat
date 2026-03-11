// SPDX-License-Identifier: Apache-2.0

//! Mempool tab -- detailed mempool statistics and transaction list

use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Cell, Paragraph, Row, Table},
	Frame,
};

use crate::mempool::{MempoolStats, RecentTxEntry};

/// Renders the mempool detail tab
pub fn render(frame: &mut Frame, area: Rect, stats: &MempoolStats, recent_txs: &[RecentTxEntry]) {
	let layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(10), // stats panel
			Constraint::Min(4),     // recent tx table
		])
		.split(area);

	#[allow(clippy::indexing_slicing)] // layout produces exactly 2 elements
	{
		render_stats(frame, layout[0], stats);
		render_recent_txs(frame, layout[1], recent_txs);
	}
}

/// Renders the mempool stats panel
fn render_stats(frame: &mut Frame, area: Rect, stats: &MempoolStats) {
	#[allow(clippy::cast_precision_loss)] // mempool size fits in f64
	#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // size fits in f64, division for MB display
	let size_mb = stats.total_size as f64 / (1024.0 * 1024.0);

	#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // total fees fits in f64, division for CAT display
	let total_fees_cat = stats.total_fees as f64 / 100_000_000.0;

	let min_rate = stats
		.min_fee_rate
		.map_or_else(|| "-".to_string(), |r| format!("{r:.1}"));
	let med_rate = stats
		.median_fee_rate
		.map_or_else(|| "-".to_string(), |r| format!("{r:.1}"));
	let max_rate = stats
		.max_fee_rate
		.map_or_else(|| "-".to_string(), |r| format!("{r:.1}"));

	let text = vec![
		Line::from(vec![
			Span::styled("Transactions: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{}", stats.count),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			),
		]),
		Line::from(vec![
			Span::styled("Size: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{size_mb:.2} / 50 MB"), Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Total fees: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{total_fees_cat:.8} CAT"), Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Fee rate (min/med/max): ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{min_rate} / {med_rate} / {max_rate} catoshis/B"),
				Style::default().fg(Color::White),
			),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Mempool Stats")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Renders the recent transactions table
fn render_recent_txs(frame: &mut Frame, area: Rect, recent_txs: &[RecentTxEntry]) {
	let header = Row::new(vec![
		Cell::from("TxID"),
		Cell::from("Size"),
		Cell::from("Fee Rate"),
		Cell::from("In"),
		Cell::from("Out"),
		Cell::from("Age"),
	])
	.style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

	let rows: Vec<Row> = recent_txs
		.iter()
		.map(|tx| {
			let txid_short = format!("{}", tx.txid);
			let txid_display = if txid_short.len() > 12 {
				// txid_short always has at least 12 chars for a hex hash
				#[allow(clippy::indexing_slicing)]
				let s = &txid_short[..12];
				s.to_string()
			} else {
				txid_short
			};

			let age_str = format_age(tx.age);

			Row::new(vec![
				Cell::from(txid_display),
				Cell::from(format!("{}", tx.size)),
				Cell::from(format!("{:.1}", tx.fee_rate)),
				Cell::from(format!("{}", tx.num_inputs)),
				Cell::from(format!("{}", tx.num_outputs)),
				Cell::from(age_str),
			])
		})
		.collect();

	let table = Table::new(
		rows,
		vec![
			Constraint::Length(14), // TxID
			Constraint::Length(7),  // Size
			Constraint::Length(9),  // Fee Rate
			Constraint::Length(4),  // In
			Constraint::Length(4),  // Out
			Constraint::Min(8),     // Age
		],
	)
	.header(header)
	.block(
		Block::default()
			.borders(Borders::ALL)
			.title("Recent Transactions")
			.border_style(Style::default().fg(Color::Cyan)),
	);

	frame.render_widget(table, area);
}

/// Formats a duration as a human-readable age string
fn format_age(duration: std::time::Duration) -> String {
	let secs = duration.as_secs();
	if secs < 60 {
		format!("{secs}s")
	} else if secs < 3600 {
		format!("{}m", secs / 60)
	} else {
		format!("{}h", secs / 3600)
	}
}
